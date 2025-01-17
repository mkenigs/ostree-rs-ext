mod fixture;

use anyhow::{Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use fn_error_context::context;
use once_cell::sync::Lazy;
use ostree_ext::container::store::PrepareResult;
use ostree_ext::container::{
    Config, ImageReference, OstreeImageReference, SignatureSource, Transport,
};
use ostree_ext::tar::TarImportOptions;
use ostree_ext::{gio, glib};
use sh_inline::bash;
use std::collections::HashMap;
use std::{io::Write, process::Command};

use fixture::Fixture;

const EXAMPLEOS_CONTENT_CHECKSUM: &str =
    "0ef7461f9db15e1d8bd8921abf20694225fbaa4462cadf7deed8ea0e43162120";
const TEST_REGISTRY_DEFAULT: &str = "localhost:5000";

fn assert_err_contains<T>(r: Result<T>, s: impl AsRef<str>) {
    let s = s.as_ref();
    let msg = format!("{:#}", r.err().unwrap());
    if !msg.contains(s) {
        panic!(r#"Error message "{}" did not contain "{}""#, msg, s);
    }
}

static TEST_REGISTRY: Lazy<String> = Lazy::new(|| match std::env::var_os("TEST_REGISTRY") {
    Some(t) => t.to_str().unwrap().to_owned(),
    None => TEST_REGISTRY_DEFAULT.to_string(),
});

#[context("Generating test tarball")]
fn initial_export(fixture: &Fixture) -> Result<Utf8PathBuf> {
    let cancellable = gio::NONE_CANCELLABLE;
    let (_, rev) = fixture
        .srcrepo
        .read_commit(fixture.testref(), cancellable)?;
    let (commitv, _) = fixture.srcrepo.load_commit(rev.as_str())?;
    assert_eq!(
        ostree::commit_get_content_checksum(&commitv)
            .unwrap()
            .as_str(),
        EXAMPLEOS_CONTENT_CHECKSUM
    );
    let destpath = fixture.path.join("exampleos-export.tar");
    let mut outf = std::io::BufWriter::new(std::fs::File::create(&destpath)?);
    let options = ostree_ext::tar::ExportOptions {
        format_version: fixture.format_version,
        ..Default::default()
    };
    ostree_ext::tar::export_commit(&fixture.srcrepo, rev.as_str(), &mut outf, Some(options))?;
    outf.flush()?;
    Ok(destpath)
}

#[tokio::test]
async fn test_tar_import_empty() -> Result<()> {
    let fixture = Fixture::new()?;
    let r = ostree_ext::tar::import_tar(&fixture.destrepo, tokio::io::empty(), None).await;
    assert_err_contains(r, "Commit object not found");
    Ok(())
}

#[tokio::test]
async fn test_tar_export_reproducible() -> Result<()> {
    let fixture = Fixture::new()?;
    let (_, rev) = fixture
        .srcrepo
        .read_commit(fixture.testref(), gio::NONE_CANCELLABLE)?;
    let export1 = {
        let mut h = openssl::hash::Hasher::new(openssl::hash::MessageDigest::sha256())?;
        ostree_ext::tar::export_commit(&fixture.srcrepo, rev.as_str(), &mut h, None)?;
        h.finish()?
    };
    // Artificial delay to flush out mtimes (one second granularity baseline, plus another 100ms for good measure).
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let export2 = {
        let mut h = openssl::hash::Hasher::new(openssl::hash::MessageDigest::sha256())?;
        ostree_ext::tar::export_commit(&fixture.srcrepo, rev.as_str(), &mut h, None)?;
        h.finish()?
    };
    assert_eq!(*export1, *export2);
    Ok(())
}

#[tokio::test]
async fn test_tar_import_signed() -> Result<()> {
    let fixture = Fixture::new()?;
    let test_tar = &initial_export(&fixture)?;

    // Verify we fail with an unknown remote.
    let src_tar = tokio::fs::File::open(test_tar).await?;
    let r = ostree_ext::tar::import_tar(
        &fixture.destrepo,
        src_tar,
        Some(TarImportOptions {
            remote: Some("nosuchremote".to_string()),
        }),
    )
    .await;
    assert_err_contains(r, r#"Remote "nosuchremote" not found"#);

    // Test a remote, but without a key
    let opts = glib::VariantDict::new(None);
    opts.insert("gpg-verify", &true);
    opts.insert("custom-backend", &"ostree-rs-ext");
    fixture
        .destrepo
        .remote_add("myremote", None, Some(&opts.end()), gio::NONE_CANCELLABLE)?;
    let src_tar = tokio::fs::File::open(test_tar).await?;
    let r = ostree_ext::tar::import_tar(
        &fixture.destrepo,
        src_tar,
        Some(TarImportOptions {
            remote: Some("myremote".to_string()),
        }),
    )
    .await;
    assert_err_contains(r, r#"Can't check signature: public key not found"#);

    // And signed correctly
    bash!(
        "ostree --repo=${repo} remote gpg-import --stdin myremote < ${p}/gpghome/key1.asc >/dev/null",
        repo = fixture.destrepo_path.as_str(),
        p = fixture.srcdir.as_str()
    )?;
    let src_tar = tokio::fs::File::open(test_tar).await?;
    let imported = ostree_ext::tar::import_tar(
        &fixture.destrepo,
        src_tar,
        Some(TarImportOptions {
            remote: Some("myremote".to_string()),
        }),
    )
    .await?;
    let (commitdata, state) = fixture.destrepo.load_commit(&imported)?;
    assert_eq!(
        EXAMPLEOS_CONTENT_CHECKSUM,
        ostree::commit_get_content_checksum(&commitdata)
            .unwrap()
            .as_str()
    );
    assert_eq!(state, ostree::RepoCommitState::NORMAL);
    Ok(())
}

#[derive(Debug)]
struct TarExpected {
    path: &'static str,
    etype: tar::EntryType,
    mode: u32,
}

impl Into<TarExpected> for &(&'static str, tar::EntryType, u32) {
    fn into(self) -> TarExpected {
        TarExpected {
            path: self.0,
            etype: self.1,
            mode: self.2,
        }
    }
}

fn validate_tar_expected<T: std::io::Read>(
    format_version: u32,
    t: tar::Entries<T>,
    expected: impl IntoIterator<Item = TarExpected>,
) -> Result<()> {
    let mut expected: HashMap<&'static str, TarExpected> =
        expected.into_iter().map(|exp| (exp.path, exp)).collect();
    let entries = t.map(|e| e.unwrap());
    // Verify we're injecting directories, fixes the absence of `/tmp` in our
    // images for example.
    for entry in entries {
        let header = entry.header();
        let entry_path = entry.path().unwrap().to_string_lossy().into_owned();
        if let Some(exp) = expected.remove(entry_path.as_str()) {
            assert_eq!(header.entry_type(), exp.etype, "{}", entry_path);
            let is_old_object = format_version == 0;
            let mut expected_mode = exp.mode;
            if is_old_object && !entry_path.starts_with("sysroot/") {
                let fmtbits = match header.entry_type() {
                    tar::EntryType::Regular => libc::S_IFREG,
                    tar::EntryType::Directory => libc::S_IFDIR,
                    tar::EntryType::Symlink => 0,
                    o => panic!("Unexpected entry type {:?}", o),
                };
                expected_mode |= fmtbits;
            }
            assert_eq!(
                header.mode().unwrap(),
                expected_mode,
                "fmtver: {} type: {:?} path: {}",
                format_version,
                header.entry_type(),
                entry_path
            );
        }
    }

    assert!(
        expected.is_empty(),
        "Expected but not found:\n{:?}",
        expected
    );
    Ok(())
}

/// Validate basic structure of the tar export.
#[test]
fn test_tar_export_structure() -> Result<()> {
    use tar::EntryType::{Directory, Regular};

    let mut fixture = Fixture::new()?;
    let src_tar = initial_export(&fixture)?;
    let src_tar = std::io::BufReader::new(std::fs::File::open(&src_tar)?);
    let mut src_tar = tar::Archive::new(src_tar);
    let mut entries = src_tar.entries()?;
    // The first entry should be the root directory.
    let first = entries.next().unwrap()?;
    let firstpath = first.path()?;
    assert_eq!(firstpath.to_str().unwrap(), "./");
    assert_eq!(first.header().mode()?, libc::S_IFDIR | 0o755);
    let next = entries.next().unwrap().unwrap();
    assert_eq!(next.path().unwrap().as_os_str(), "sysroot");

    // Validate format version 0
    let expected = [
        ("sysroot/config", Regular, 0o644),
        ("sysroot/ostree/repo", Directory, 0o755),
        ("sysroot/ostree/repo/objects/00", Directory, 0o755),
        ("sysroot/ostree/repo/objects/23", Directory, 0o755),
        ("sysroot/ostree/repo/objects/77", Directory, 0o755),
        ("sysroot/ostree/repo/objects/bc", Directory, 0o755),
        ("sysroot/ostree/repo/objects/ff", Directory, 0o755),
        ("sysroot/ostree/repo/refs", Directory, 0o755),
        ("sysroot/ostree/repo/refs", Directory, 0o755),
        ("sysroot/ostree/repo/refs/heads", Directory, 0o755),
        ("sysroot/ostree/repo/refs/mirrors", Directory, 0o755),
        ("sysroot/ostree/repo/refs/remotes", Directory, 0o755),
        ("sysroot/ostree/repo/tmp", Directory, 0o755),
        ("sysroot/ostree/repo/tmp/cache", Directory, 0o755),
        ("sysroot/ostree/repo/xattrs", Directory, 0o755),
        ("usr", Directory, 0o755),
    ];
    validate_tar_expected(
        fixture.format_version,
        entries,
        expected.iter().map(Into::into),
    )?;

    // Validate format version 1
    fixture.format_version = 1;
    let src_tar = initial_export(&fixture)?;
    let src_tar = std::io::BufReader::new(std::fs::File::open(&src_tar)?);
    let mut src_tar = tar::Archive::new(src_tar);
    let expected = [
        ("sysroot/ostree/repo", Directory, 0o755),
        ("sysroot/ostree/repo/config", Regular, 0o644),
        ("sysroot/ostree/repo/objects/00", Directory, 0o755),
        ("sysroot/ostree/repo/objects/23", Directory, 0o755),
        ("sysroot/ostree/repo/objects/77", Directory, 0o755),
        ("sysroot/ostree/repo/objects/bc", Directory, 0o755),
        ("sysroot/ostree/repo/objects/ff", Directory, 0o755),
        ("sysroot/ostree/repo/refs", Directory, 0o755),
        ("sysroot/ostree/repo/refs", Directory, 0o755),
        ("sysroot/ostree/repo/refs/heads", Directory, 0o755),
        ("sysroot/ostree/repo/refs/mirrors", Directory, 0o755),
        ("sysroot/ostree/repo/refs/remotes", Directory, 0o755),
        ("sysroot/ostree/repo/tmp", Directory, 0o755),
        ("sysroot/ostree/repo/tmp/cache", Directory, 0o755),
        ("usr", Directory, 0o755),
    ];
    validate_tar_expected(
        fixture.format_version,
        src_tar.entries()?,
        expected.iter().map(Into::into),
    )?;

    Ok(())
}

#[tokio::test]
async fn test_tar_import_export() -> Result<()> {
    let fixture = Fixture::new()?;
    let p = &initial_export(&fixture)?;
    let src_tar = tokio::fs::File::open(p).await?;

    let imported_commit: String =
        ostree_ext::tar::import_tar(&fixture.destrepo, src_tar, None).await?;
    let (commitdata, _) = fixture.destrepo.load_commit(&imported_commit)?;
    assert_eq!(
        EXAMPLEOS_CONTENT_CHECKSUM,
        ostree::commit_get_content_checksum(&commitdata)
            .unwrap()
            .as_str()
    );
    bash!(
        r#"
         ostree --repo=${destrepodir} ls -R ${imported_commit} >/dev/null
         val=$(ostree --repo=${destrepodir} show --print-detached-metadata-key=my-detached-key ${imported_commit})
         test "${val}" = "'my-detached-value'"
        "#,
        destrepodir = fixture.destrepo_path.as_str(),
        imported_commit = imported_commit.as_str()
    )?;
    Ok(())
}

#[tokio::test]
async fn test_tar_write() -> Result<()> {
    let fixture = Fixture::new()?;
    // Test translating /etc to /usr/etc
    let tmpetc = fixture.path.join("tmproot/etc");
    std::fs::create_dir_all(&tmpetc)?;
    std::fs::write(tmpetc.join("someconfig.conf"), b"")?;
    let tmproot = tmpetc.parent().unwrap();
    let tmpvarlib = &tmproot.join("var/lib");
    std::fs::create_dir_all(tmpvarlib)?;
    std::fs::write(tmpvarlib.join("foo.log"), "foolog")?;
    std::fs::write(tmpvarlib.join("bar.log"), "barlog")?;
    std::fs::create_dir_all(tmproot.join("boot"))?;
    let tmptar = fixture.path.join("testlayer.tar");
    bash!(
        "tar cf ${tmptar} -C ${tmproot} .",
        tmptar = tmptar.as_str(),
        tmproot = tmproot.as_str()
    )?;
    let src = tokio::fs::File::open(&tmptar).await?;
    let r = ostree_ext::tar::write_tar(&fixture.destrepo, src, "layer", None).await?;
    bash!(
        "ostree --repo=${repo} ls ${layer_commit} /usr/etc/someconfig.conf >/dev/null",
        repo = fixture.destrepo_path.as_str(),
        layer_commit = r.commit.as_str()
    )?;
    assert_eq!(r.filtered.len(), 2);
    assert_eq!(*r.filtered.get("var").unwrap(), 4);
    assert_eq!(*r.filtered.get("boot").unwrap(), 1);

    Ok(())
}

fn skopeo_inspect(imgref: &str) -> Result<String> {
    let out = Command::new("skopeo")
        .args(&["inspect", imgref])
        .stdout(std::process::Stdio::piped())
        .output()?;
    Ok(String::from_utf8(out.stdout)?)
}

fn skopeo_inspect_config(imgref: &str) -> Result<oci_spec::image::ImageConfiguration> {
    let out = Command::new("skopeo")
        .args(&["inspect", "--config", imgref])
        .stdout(std::process::Stdio::piped())
        .output()?;
    Ok(serde_json::from_slice(&out.stdout)?)
}

#[tokio::test]
async fn test_container_import_export() -> Result<()> {
    let fixture = Fixture::new()?;
    let testrev = fixture
        .srcrepo
        .require_rev(fixture.testref())
        .context("Failed to resolve ref")?;

    let srcoci_path = &fixture.path.join("oci");
    let srcoci_imgref = ImageReference {
        transport: Transport::OciDir,
        name: srcoci_path.as_str().to_string(),
    };
    let config = Config {
        labels: Some(
            [("foo", "bar"), ("test", "value")]
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        ),
        ..Default::default()
    };
    let opts = ostree_ext::container::ExportOpts {
        copy_meta_keys: vec!["buildsys.checksum".to_string()],
        ..Default::default()
    };
    let digest = ostree_ext::container::encapsulate(
        &fixture.srcrepo,
        fixture.testref(),
        &config,
        Some(opts),
        &srcoci_imgref,
    )
    .await
    .context("exporting")?;
    assert!(srcoci_path.exists());

    let inspect = skopeo_inspect(&srcoci_imgref.to_string())?;
    assert!(inspect.contains(r#""version": "42.0""#));
    assert!(inspect.contains(r#""foo": "bar""#));
    assert!(inspect.contains(r#""test": "value""#));
    assert!(inspect.contains(
        r#""buildsys.checksum": "41af286dc0b172ed2f1ca934fd2278de4a1192302ffa07087cea2682e7d372e3""#
    ));
    let cfg = skopeo_inspect_config(&srcoci_imgref.to_string())?;
    // unwrap.  Unwrap.  UnWrap.  UNWRAP!!!!!!!
    assert_eq!(
        cfg.config()
            .as_ref()
            .unwrap()
            .cmd()
            .as_ref()
            .unwrap()
            .get(0)
            .as_ref()
            .unwrap()
            .as_str(),
        "/usr/bin/bash"
    );

    let srcoci_unverified = OstreeImageReference {
        sigverify: SignatureSource::ContainerPolicyAllowInsecure,
        imgref: srcoci_imgref.clone(),
    };

    let (_, pushed_digest) = ostree_ext::container::fetch_manifest(&srcoci_unverified).await?;
    assert_eq!(pushed_digest, digest);

    // No remote matching
    let srcoci_unknownremote = OstreeImageReference {
        sigverify: SignatureSource::OstreeRemote("unknownremote".to_string()),
        imgref: srcoci_imgref.clone(),
    };
    let r = ostree_ext::container::unencapsulate(&fixture.destrepo, &srcoci_unknownremote, None)
        .await
        .context("importing");
    assert_err_contains(r, r#"Remote "unknownremote" not found"#);

    // Test with a signature
    let opts = glib::VariantDict::new(None);
    opts.insert("gpg-verify", &true);
    opts.insert("custom-backend", &"ostree-rs-ext");
    fixture
        .destrepo
        .remote_add("myremote", None, Some(&opts.end()), gio::NONE_CANCELLABLE)?;
    bash!(
        "ostree --repo=${repo} remote gpg-import --stdin myremote < ${p}/gpghome/key1.asc",
        repo = fixture.destrepo_path.as_str(),
        p = fixture.srcdir.as_str()
    )?;

    // No remote matching
    let srcoci_verified = OstreeImageReference {
        sigverify: SignatureSource::OstreeRemote("myremote".to_string()),
        imgref: srcoci_imgref.clone(),
    };
    let import = ostree_ext::container::unencapsulate(&fixture.destrepo, &srcoci_verified, None)
        .await
        .context("importing")?;
    assert_eq!(import.ostree_commit, testrev.as_str());

    // Test without signature verification
    // Create a new repo
    {
        let fixture = Fixture::new()?;
        let import =
            ostree_ext::container::unencapsulate(&fixture.destrepo, &srcoci_unverified, None)
                .await
                .context("importing")?;
        assert_eq!(import.ostree_commit, testrev.as_str());
    }

    Ok(())
}

/// Copy an OCI directory.
async fn oci_clone(src: impl AsRef<Utf8Path>, dest: impl AsRef<Utf8Path>) -> Result<()> {
    let src = src.as_ref();
    let dest = dest.as_ref();
    // For now we just fork off `cp` and rely on reflinks, but we could and should
    // explicitly hardlink blobs/sha256 e.g.
    let cmd = tokio::process::Command::new("cp")
        .args(&["-a", "--reflink=auto"])
        .args(&[src, dest])
        .status()
        .await?;
    if !cmd.success() {
        anyhow::bail!("cp failed");
    }
    Ok(())
}

/// But layers work via the container::write module.
#[tokio::test]
async fn test_container_write_derive() -> Result<()> {
    let fixture = Fixture::new()?;
    let base_oci_path = &fixture.path.join("exampleos.oci");
    let _digest = ostree_ext::container::encapsulate(
        &fixture.srcrepo,
        fixture.testref(),
        &Config {
            cmd: Some(vec!["/bin/bash".to_string()]),
            ..Default::default()
        },
        None,
        &ImageReference {
            transport: Transport::OciDir,
            name: base_oci_path.to_string(),
        },
    )
    .await
    .context("exporting")?;
    assert!(base_oci_path.exists());

    // Build the derived images
    let derived_path = &fixture.path.join("derived.oci");
    oci_clone(base_oci_path, derived_path).await?;
    let temproot = &fixture.path.join("temproot");
    std::fs::create_dir_all(&temproot.join("usr/bin"))?;
    std::fs::write(temproot.join("usr/bin/newderivedfile"), "newderivedfile v0")?;
    std::fs::write(
        temproot.join("usr/bin/newderivedfile3"),
        "newderivedfile3 v0",
    )?;
    ostree_ext::integrationtest::generate_derived_oci(derived_path, temproot)?;
    // And v2
    let derived2_path = &fixture.path.join("derived2.oci");
    oci_clone(base_oci_path, derived2_path).await?;
    std::fs::remove_dir_all(temproot)?;
    std::fs::create_dir_all(&temproot.join("usr/bin"))?;
    std::fs::write(temproot.join("usr/bin/newderivedfile"), "newderivedfile v1")?;
    std::fs::write(
        temproot.join("usr/bin/newderivedfile2"),
        "newderivedfile2 v0",
    )?;
    ostree_ext::integrationtest::generate_derived_oci(derived2_path, temproot)?;

    let derived_ref = OstreeImageReference {
        sigverify: SignatureSource::ContainerPolicyAllowInsecure,
        imgref: ImageReference {
            transport: Transport::OciDir,
            name: derived_path.to_string(),
        },
    };
    // There shouldn't be any container images stored yet.
    let images = ostree_ext::container::store::list_images(&fixture.destrepo)?;
    assert!(images.is_empty());

    // Verify importing a derive dimage fails
    let r = ostree_ext::container::unencapsulate(&fixture.destrepo, &derived_ref, None).await;
    assert_err_contains(r, "Expected 1 layer, found 2");

    // Pull a derived image - two layers, new base plus one layer.
    let mut imp = ostree_ext::container::store::LayeredImageImporter::new(
        &fixture.destrepo,
        &derived_ref,
        Default::default(),
    )
    .await?;
    let prep = match imp.prepare().await? {
        PrepareResult::AlreadyPresent(_) => panic!("should not be already imported"),
        PrepareResult::Ready(r) => r,
    };
    let expected_digest = prep.manifest_digest.clone();
    assert!(prep.base_layer.commit.is_none());
    assert_eq!(prep.layers.len(), 1);
    for layer in prep.layers.iter() {
        assert!(layer.commit.is_none());
    }
    let import = imp.import(prep).await?;
    // We should have exactly one image stored.
    let images = ostree_ext::container::store::list_images(&fixture.destrepo)?;
    assert_eq!(images.len(), 1);
    assert_eq!(images[0], derived_ref.imgref.to_string());

    let imported_commit = &fixture
        .destrepo
        .load_commit(import.merge_commit.as_str())?
        .0;
    let digest = ostree_ext::container::store::manifest_digest_from_commit(imported_commit)?;
    assert!(digest.starts_with("sha256:"));
    assert_eq!(digest, expected_digest);

    #[cfg(feature = "proxy_v0_2_3")]
    {
        let commit_meta = &imported_commit.child_value(0);
        let proxy = containers_image_proxy::ImageProxy::new().await?;
        let commit_meta = glib::VariantDict::new(Some(commit_meta));
        let config = commit_meta
            .lookup::<String>("ostree.container.image-config")?
            .unwrap();
        let config: oci_spec::image::ImageConfiguration = serde_json::from_str(&config)?;
        assert_eq!(config.os(), &oci_spec::image::Os::Linux);
    }

    // Parse the commit and verify we pulled the derived content.
    bash!(
        "ostree --repo=${repo} ls ${r} /usr/bin/newderivedfile >/dev/null",
        repo = fixture.destrepo_path.as_str(),
        r = import.merge_commit.as_str()
    )?;

    // Import again, but there should be no changes.
    let mut imp = ostree_ext::container::store::LayeredImageImporter::new(
        &fixture.destrepo,
        &derived_ref,
        Default::default(),
    )
    .await?;
    let already_present = match imp.prepare().await? {
        PrepareResult::AlreadyPresent(c) => c,
        PrepareResult::Ready(_) => {
            panic!("Should have already imported {}", &derived_ref)
        }
    };
    assert_eq!(import.merge_commit, already_present.merge_commit);

    // Test upgrades; replace the oci-archive with new content.
    std::fs::remove_dir_all(derived_path)?;
    std::fs::rename(derived2_path, derived_path)?;
    let mut imp = ostree_ext::container::store::LayeredImageImporter::new(
        &fixture.destrepo,
        &derived_ref,
        Default::default(),
    )
    .await?;
    let prep = match imp.prepare().await? {
        PrepareResult::AlreadyPresent(_) => panic!("should not be already imported"),
        PrepareResult::Ready(r) => r,
    };
    // We *should* already have the base layer.
    assert!(prep.base_layer.commit.is_some());
    // One new layer
    assert_eq!(prep.layers.len(), 1);
    for layer in prep.layers.iter() {
        assert!(layer.commit.is_none());
    }
    let import = imp.import(prep).await?;
    // New commit.
    assert_ne!(import.merge_commit, already_present.merge_commit);
    // We should still have exactly one image stored.
    let images = ostree_ext::container::store::list_images(&fixture.destrepo)?;
    assert_eq!(images[0], derived_ref.imgref.to_string());
    assert_eq!(images.len(), 1);

    // Verify we have the new file and *not* the old one
    bash!(
        r#"set -x;
         ostree --repo=${repo} ls ${r} /usr/bin/newderivedfile2 >/dev/null
         test "$(ostree --repo=${repo} cat ${r} /usr/bin/newderivedfile)" = "newderivedfile v1"
         if ostree --repo=${repo} ls ${r} /usr/bin/newderivedfile3 2>/dev/null; then
           echo oops; exit 1
         fi
        "#,
        repo = fixture.destrepo_path.as_str(),
        r = import.merge_commit.as_str()
    )?;

    // And there should be no changes on upgrade again.
    let mut imp = ostree_ext::container::store::LayeredImageImporter::new(
        &fixture.destrepo,
        &derived_ref,
        Default::default(),
    )
    .await?;
    let already_present = match imp.prepare().await? {
        PrepareResult::AlreadyPresent(c) => c,
        PrepareResult::Ready(_) => {
            panic!("Should have already imported {}", &derived_ref)
        }
    };
    assert_eq!(import.merge_commit, already_present.merge_commit);

    // Create a new repo, and copy to it
    let destrepo2 = ostree::Repo::create_at(
        ostree::AT_FDCWD,
        fixture.path.join("destrepo2").as_str(),
        ostree::RepoMode::Archive,
        None,
        gio::NONE_CANCELLABLE,
    )?;
    ostree_ext::container::store::copy(&fixture.destrepo, &destrepo2, &derived_ref).await?;

    let images = ostree_ext::container::store::list_images(&destrepo2)?;
    assert_eq!(images.len(), 1);
    assert_eq!(images[0], derived_ref.imgref.to_string());

    Ok(())
}

#[ignore]
#[tokio::test]
// Verify that we can push and pull to a registry, not just oci-archive:.
// This requires a registry set up externally right now.  One can run a HTTP registry via e.g.
// `podman run --rm -ti -p 5000:5000 --name registry docker.io/library/registry:2`
// but that doesn't speak HTTPS and adding that is complex.
// A simple option is setting up e.g. quay.io/$myuser/exampleos and then do:
// Then you can run this test via `env TEST_REGISTRY=quay.io/$myuser cargo test -- --ignored`.
async fn test_container_import_export_registry() -> Result<()> {
    let tr = &*TEST_REGISTRY;
    let fixture = Fixture::new()?;
    let testref = fixture.testref();
    let testrev = fixture
        .srcrepo
        .require_rev(testref)
        .context("Failed to resolve ref")?;
    let src_imgref = ImageReference {
        transport: Transport::Registry,
        name: format!("{}/exampleos", tr),
    };
    let config = Config {
        cmd: Some(vec!["/bin/bash".to_string()]),
        ..Default::default()
    };
    let digest =
        ostree_ext::container::encapsulate(&fixture.srcrepo, testref, &config, None, &src_imgref)
            .await
            .context("exporting to registry")?;
    let mut digested_imgref = src_imgref.clone();
    digested_imgref.name = format!("{}@{}", src_imgref.name, digest);

    let import_ref = OstreeImageReference {
        sigverify: SignatureSource::ContainerPolicyAllowInsecure,
        imgref: digested_imgref,
    };
    let import = ostree_ext::container::unencapsulate(&fixture.destrepo, &import_ref, None)
        .await
        .context("importing")?;
    assert_eq!(import.ostree_commit, testrev.as_str());
    Ok(())
}

#[test]
fn test_diff() -> Result<()> {
    let mut fixture = Fixture::new()?;
    fixture.update()?;
    let from = &format!("{}^", fixture.testref());
    let repo = &fixture.srcrepo;
    let subdir: Option<&str> = None;
    let diff = ostree_ext::diff::diff(repo, from, fixture.testref(), subdir)?;
    assert!(diff.subdir.is_none());
    assert_eq!(diff.added_dirs.len(), 1);
    assert_eq!(diff.added_dirs.iter().next().unwrap(), "/usr/share");
    assert_eq!(diff.added_files.len(), 1);
    assert_eq!(diff.added_files.iter().next().unwrap(), "/usr/bin/newbin");
    assert_eq!(diff.removed_files.len(), 1);
    assert_eq!(diff.removed_files.iter().next().unwrap(), "/usr/bin/foo");
    let diff = ostree_ext::diff::diff(repo, from, fixture.testref(), Some("/usr"))?;
    assert_eq!(diff.subdir.as_ref().unwrap(), "/usr");
    assert_eq!(diff.added_dirs.len(), 1);
    assert_eq!(diff.added_dirs.iter().next().unwrap(), "/share");
    assert_eq!(diff.added_files.len(), 1);
    assert_eq!(diff.added_files.iter().next().unwrap(), "/bin/newbin");
    assert_eq!(diff.removed_files.len(), 1);
    assert_eq!(diff.removed_files.iter().next().unwrap(), "/bin/foo");
    Ok(())
}
