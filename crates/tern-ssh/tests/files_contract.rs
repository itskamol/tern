//! The file source contract, run against both adapters: the Mac's file system on a temp dir and
//! SFTP against the in-process server. A test copies, renames and deletes through the source and
//! looks at what is left on disk; it does not look at the calls made.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::path::{Path, PathBuf};

use common::{serve_sftp, spec, start};
use tern_ssh::{FileError, FileSource, LocalFs, SftpFiles};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Bytes that differ from one position to the next, 300001 long: not a multiple of any chunk.
fn pattern() -> Vec<u8> {
    (0..300_001u32)
        .map(|i| (i.wrapping_mul(31) % 251) as u8)
        .collect()
}

/// A source with a folder to work in: `dir` as the source names it, `disk` where it really is.
struct Rig {
    source: Box<dyn FileSource>,
    dir: String,
    disk: PathBuf,
    trash: Option<PathBuf>,
    _keep: Vec<Box<dyn std::any::Any>>,
}

async fn mac() -> Rig {
    let root = tempfile::tempdir().unwrap();
    let trash = tempfile::tempdir().unwrap();
    let disk = root.path().join("work");
    std::fs::create_dir(&disk).unwrap();
    Rig {
        source: Box::new(LocalFs::with_trash(trash.path().to_path_buf())),
        dir: disk.to_str().unwrap().to_owned(),
        disk,
        trash: Some(trash.path().to_path_buf()),
        _keep: vec![Box::new(root), Box::new(trash)],
    }
}

async fn host() -> Rig {
    let remote = tempfile::tempdir().unwrap();
    std::fs::create_dir(remote.path().join("home")).unwrap();
    let keys = tempfile::tempdir().unwrap();
    let server = serve_sftp(remote.path().to_path_buf()).await;
    let live = start(spec(server.port, &keys)).await.unwrap();
    let sftp = live.handle.open_sftp().await.unwrap();
    Rig {
        source: Box::new(SftpFiles::new("host", sftp)),
        dir: "/home".to_owned(),
        disk: remote.path().join("home"),
        trash: None,
        _keep: vec![Box::new(remote), Box::new(keys), Box::new(live)],
    }
}

fn at(rig: &Rig, name: &str) -> String {
    format!("{}/{name}", rig.dir)
}

async fn list_puts_folders_first_then_sorts_by_name(rig: Rig) {
    std::fs::write(rig.disk.join("b.txt"), b"12345").unwrap();
    std::fs::write(rig.disk.join("A.txt"), b"").unwrap();
    std::fs::create_dir(rig.disk.join("zeta")).unwrap();
    std::fs::create_dir(rig.disk.join("alpha")).unwrap();
    let got = rig.source.list(&rig.dir).await.unwrap();
    let names: Vec<_> = got.iter().map(|x| x.name.as_str()).collect();
    assert_eq!(names, ["alpha", "zeta", "A.txt", "b.txt"]);
    assert_eq!(
        got.iter().map(|x| x.is_dir).collect::<Vec<_>>(),
        [true, true, false, false]
    );
    let b = &got[3];
    assert_eq!((b.size, b.path.as_str()), (5, at(&rig, "b.txt").as_str()));
    assert!(b.modified.is_some());
}

async fn a_missing_folder_or_file_is_not_found(rig: Rig) {
    let missing = at(&rig, "nope");
    assert_eq!(
        rig.source.list(&missing).await.unwrap_err(),
        FileError::NotFound(missing.clone())
    );
    assert!(matches!(
        rig.source.read(&missing).await.err().unwrap(),
        FileError::NotFound(_)
    ));
}

async fn what_is_written_reads_back_byte_for_byte(rig: Rig) {
    let data = pattern();
    let path = at(&rig, "big.bin");
    let mut out = rig.source.write(&path).await.unwrap();
    out.write_all(&data).await.unwrap();
    out.shutdown().await.unwrap();
    assert_eq!(std::fs::read(rig.disk.join("big.bin")).unwrap(), data);
    let mut back = Vec::new();
    rig.source
        .read(&path)
        .await
        .unwrap()
        .read_to_end(&mut back)
        .await
        .unwrap();
    assert_eq!(back, data);
}

async fn writing_over_a_file_empties_it_first(rig: Rig) {
    std::fs::write(rig.disk.join("f"), b"a long old content").unwrap();
    let mut out = rig.source.write(&at(&rig, "f")).await.unwrap();
    out.write_all(b"new").await.unwrap();
    out.shutdown().await.unwrap();
    assert_eq!(std::fs::read(rig.disk.join("f")).unwrap(), b"new");
}

async fn mkdir_makes_one_folder_and_refuses_a_taken_name(rig: Rig) {
    let path = at(&rig, "made");
    rig.source.mkdir(&path).await.unwrap();
    assert!(rig.disk.join("made").is_dir());
    assert_eq!(
        rig.source.mkdir(&path).await.unwrap_err(),
        FileError::AlreadyExists(path)
    );
}

async fn rename_moves_and_never_replaces(rig: Rig) {
    std::fs::write(rig.disk.join("a"), b"A").unwrap();
    std::fs::write(rig.disk.join("b"), b"B").unwrap();
    rig.source
        .rename(&at(&rig, "a"), &at(&rig, "c"))
        .await
        .unwrap();
    assert!(!rig.disk.join("a").exists());
    assert_eq!(std::fs::read(rig.disk.join("c")).unwrap(), b"A");
    let err = rig
        .source
        .rename(&at(&rig, "c"), &at(&rig, "b"))
        .await
        .unwrap_err();
    assert_eq!(err, FileError::AlreadyExists(at(&rig, "b")));
    assert_eq!(std::fs::read(rig.disk.join("b")).unwrap(), b"B");
    assert_eq!(std::fs::read(rig.disk.join("c")).unwrap(), b"A");
}

async fn delete_removes_a_file_and_a_folder_with_everything_in_it(rig: Rig) {
    std::fs::write(rig.disk.join("f"), b"x").unwrap();
    std::fs::create_dir_all(rig.disk.join("d/inner/deep")).unwrap();
    std::fs::write(rig.disk.join("d/one"), b"1").unwrap();
    std::fs::write(rig.disk.join("d/inner/deep/two"), b"2").unwrap();
    rig.source.delete(&at(&rig, "f")).await.unwrap();
    rig.source.delete(&at(&rig, "d")).await.unwrap();
    assert_eq!(std::fs::read_dir(&rig.disk).unwrap().count(), 0);
    assert!(matches!(
        rig.source.delete(&at(&rig, "d")).await.unwrap_err(),
        FileError::NotFound(_)
    ));
}

macro_rules! contract {
    ($($name:ident),* $(,)?) => {
        mod on_the_mac {
            use super::*;
            $(
                #[tokio::test(flavor = "multi_thread")]
                async fn $name() {
                    super::$name(mac().await).await;
                }
            )*
        }
        mod on_a_host {
            use super::*;
            $(
                #[tokio::test(flavor = "multi_thread")]
                async fn $name() {
                    super::$name(host().await).await;
                }
            )*
        }
    };
}

contract!(
    list_puts_folders_first_then_sorts_by_name,
    a_missing_folder_or_file_is_not_found,
    what_is_written_reads_back_byte_for_byte,
    writing_over_a_file_empties_it_first,
    mkdir_makes_one_folder_and_refuses_a_taken_name,
    rename_moves_and_never_replaces,
    delete_removes_a_file_and_a_folder_with_everything_in_it,
);

#[tokio::test(flavor = "multi_thread")]
async fn a_host_names_its_login_folder_as_home() {
    let rig = host().await;
    assert_eq!(rig.source.home().await.unwrap(), "/home");
    assert_eq!(rig.source.id(), "host");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_mac_deletes_into_the_trash_and_numbers_a_name_already_there() {
    let rig = mac().await;
    let trash = rig.trash.clone().unwrap();
    std::fs::write(rig.disk.join("a.txt"), b"first").unwrap();
    std::fs::create_dir(rig.disk.join("d")).unwrap();
    std::fs::write(rig.disk.join("d/in"), b"inside").unwrap();
    rig.source.delete(&at(&rig, "a.txt")).await.unwrap();
    std::fs::write(rig.disk.join("a.txt"), b"second").unwrap();
    rig.source.delete(&at(&rig, "a.txt")).await.unwrap();
    rig.source.delete(&at(&rig, "d")).await.unwrap();
    assert!(!rig.disk.join("a.txt").exists());
    assert_eq!(std::fs::read(trash.join("a.txt")).unwrap(), b"first");
    assert_eq!(std::fs::read(trash.join("a 2.txt")).unwrap(), b"second");
    assert_eq!(std::fs::read(trash.join("d/in")).unwrap(), b"inside");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_mac_starts_in_the_home_folder() {
    let home = LocalFs::with_trash(PathBuf::from("/nonexistent"))
        .home()
        .await
        .unwrap();
    assert!(Path::new(&home).is_dir(), "{home}");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_mac_removes_a_part_file_outright_instead_of_trashing_it() {
    let rig = mac().await;
    let trash = rig.trash.clone().unwrap();
    std::fs::write(rig.disk.join("big.bin.part"), b"half").unwrap();
    rig.source.delete(&at(&rig, "big.bin.part")).await.unwrap();
    assert!(!rig.disk.join("big.bin.part").exists());
    assert_eq!(std::fs::read_dir(&trash).unwrap().count(), 0);
}
