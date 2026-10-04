//! Unit tests of the in-memory filesystem. They drive `memfs` / `memwalk` /
//! `memtokio` directly (compiled for tests whatever the feature), on the
//! process-wide default backend, each under its own root so they can run in
//! parallel.

use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::memfs as fs;
use crate::memory::{Backend, MemoryBackend};
use crate::memwalk::WalkDir;
use crate::{lexical, normalize};

fn root(name: &str) -> PathBuf {
    let r = PathBuf::from(format!("/dbt-vfs-test/{name}"));
    fs::create_dir_all(&r).unwrap();
    r
}

fn names(dir: &Path) -> Vec<String> {
    fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect()
}

#[test]
fn write_read_round_trip() {
    let r = root("rw");
    fs::write(r.join("a.txt"), b"hello").unwrap();
    assert_eq!(fs::read(r.join("a.txt")).unwrap(), b"hello");
    assert_eq!(fs::read_to_string(r.join("a.txt")).unwrap(), "hello");
    // write truncates
    fs::write(r.join("a.txt"), b"hi").unwrap();
    assert_eq!(fs::read_to_string(r.join("a.txt")).unwrap(), "hi");
    let m = fs::metadata(r.join("a.txt")).unwrap();
    assert!(m.is_file() && !m.is_dir());
    assert_eq!(m.len(), 2);
    // invalid UTF-8 is InvalidData, as std
    fs::write(r.join("bin"), [0xff, 0xfe]).unwrap();
    assert_eq!(
        fs::read_to_string(r.join("bin")).unwrap_err().kind(),
        io::ErrorKind::InvalidData
    );
}

#[test]
fn missing_files_and_parents() {
    let r = root("missing");
    assert_eq!(
        fs::read(r.join("nope")).unwrap_err().kind(),
        io::ErrorKind::NotFound
    );
    // like std: no file without its parent directory
    assert_eq!(
        fs::write(r.join("no/such/dir/f"), b"x").unwrap_err().kind(),
        io::ErrorKind::NotFound
    );
    assert!(!fs::exists(r.join("nope")).unwrap());
    // a file is not a directory
    fs::write(r.join("f"), b"x").unwrap();
    assert!(fs::write(r.join("f/g"), b"x").is_err());
    assert!(fs::create_dir_all(r.join("f/g")).is_err());
}

#[test]
fn file_handle_read_write_seek_append() {
    let r = root("handle");
    let p = r.join("f");
    {
        let mut f = fs::File::create(&p).unwrap();
        f.write_all(b"0123456789").unwrap();
        f.seek(SeekFrom::Start(2)).unwrap();
        f.write_all(b"ab").unwrap();
        f.seek(SeekFrom::End(2)).unwrap();
        f.write_all(b"Z").unwrap(); // past the end: zero-filled gap
    }
    assert_eq!(fs::read(&p).unwrap(), b"01ab456789\0\0Z");
    {
        let mut f = fs::OpenOptions::new().append(true).open(&p).unwrap();
        f.seek(SeekFrom::Start(0)).unwrap();
        f.write_all(b"!").unwrap(); // append ignores the position
    }
    assert_eq!(fs::read(&p).unwrap(), b"01ab456789\0\0Z!");
    let mut f = fs::File::open(&p).unwrap();
    let mut buf = [0u8; 4];
    f.seek(SeekFrom::Current(1)).unwrap();
    f.read_exact(&mut buf).unwrap();
    assert_eq!(&buf, b"1ab4");
    // read-only handle refuses writes; write-only refuses reads
    assert!(f.write(b"x").is_err());
    let mut w = fs::OpenOptions::new().write(true).open(&p).unwrap();
    assert!(w.read(&mut buf).is_err());
    // set_len truncates and extends
    w.set_len(3).unwrap();
    assert_eq!(fs::read(&p).unwrap(), b"01a");
    assert_eq!(w.metadata().unwrap().len(), 3);
    // two handles share the bytes
    let mut a = fs::OpenOptions::new().read(true).write(true).open(&p).unwrap();
    let mut b = a.try_clone().unwrap();
    a.seek(SeekFrom::End(0)).unwrap();
    a.write_all(b"X").unwrap();
    let mut s = String::new();
    b.seek(SeekFrom::Start(0)).unwrap();
    b.read_to_string(&mut s).unwrap();
    assert_eq!(s, "01aX");
}

#[test]
fn open_options_follow_std() {
    let r = root("opts");
    let p = r.join("f");
    assert_eq!(
        fs::OpenOptions::new().open(&p).unwrap_err().kind(),
        io::ErrorKind::InvalidInput
    );
    assert_eq!(
        fs::OpenOptions::new().read(true).create(true).open(&p).unwrap_err().kind(),
        io::ErrorKind::InvalidInput
    );
    fs::File::create_new(&p).unwrap();
    assert_eq!(
        fs::File::create_new(&p).unwrap_err().kind(),
        io::ErrorKind::AlreadyExists
    );
    assert_eq!(
        fs::File::open(&r).unwrap_err().kind(),
        io::ErrorKind::IsADirectory
    );
}

#[test]
fn directories() {
    let r = root("dirs");
    fs::create_dir(r.join("a")).unwrap();
    assert_eq!(
        fs::create_dir(r.join("a")).unwrap_err().kind(),
        io::ErrorKind::AlreadyExists
    );
    assert_eq!(
        fs::create_dir(r.join("x/y")).unwrap_err().kind(),
        io::ErrorKind::NotFound
    );
    fs::create_dir_all(r.join("a/b/c")).unwrap();
    fs::create_dir_all(r.join("a/b/c")).unwrap(); // idempotent
    assert!(fs::metadata(r.join("a/b")).unwrap().is_dir());
    fs::write(r.join("a/b/c/f"), b"1").unwrap();
    assert_eq!(
        fs::remove_dir(r.join("a/b")).unwrap_err().kind(),
        io::ErrorKind::DirectoryNotEmpty
    );
    assert_eq!(
        fs::remove_file(r.join("a")).unwrap_err().kind(),
        io::ErrorKind::IsADirectory
    );
    fs::remove_file(r.join("a/b/c/f")).unwrap();
    fs::remove_dir(r.join("a/b/c")).unwrap();
    assert!(!fs::exists(r.join("a/b/c")).unwrap());
    fs::create_dir_all(r.join("a/b/d/e")).unwrap();
    fs::write(r.join("a/b/d/e/f"), b"1").unwrap();
    fs::remove_dir_all(r.join("a")).unwrap();
    assert!(!fs::exists(r.join("a")).unwrap());
    assert!(fs::exists(&r).unwrap());
}

#[test]
fn read_dir_is_sorted_and_direct() {
    let r = root("readdir");
    for n in ["zeta", "alpha", "Mid", "beta.sql"] {
        fs::write(r.join(n), b"").unwrap();
    }
    fs::create_dir_all(r.join("sub/deep")).unwrap();
    fs::write(r.join("sub/deep/x"), b"").unwrap();
    assert_eq!(names(&r), ["Mid", "alpha", "beta.sql", "sub", "zeta"]);
    let sub = fs::read_dir(&r)
        .unwrap()
        .map(|e| e.unwrap())
        .find(|e| e.file_name() == "sub")
        .unwrap();
    assert!(sub.file_type().unwrap().is_dir());
    assert_eq!(sub.path(), r.join("sub"));
    assert_eq!(names(&r.join("sub")), ["deep"]);
    assert_eq!(
        fs::read_dir(r.join("zeta")).unwrap_err().kind(),
        io::ErrorKind::NotADirectory
    );
}

#[test]
fn rename_files_and_directories() {
    let r = root("rename");
    fs::write(r.join("a"), b"A").unwrap();
    fs::write(r.join("b"), b"B").unwrap();
    fs::rename(r.join("a"), r.join("b")).unwrap(); // replaces
    assert_eq!(fs::read(r.join("b")).unwrap(), b"A");
    assert!(!fs::exists(r.join("a")).unwrap());

    fs::create_dir_all(r.join("d1/x")).unwrap();
    fs::write(r.join("d1/x/f"), b"F").unwrap();
    fs::rename(r.join("d1"), r.join("d2")).unwrap();
    assert_eq!(fs::read(r.join("d2/x/f")).unwrap(), b"F");
    assert!(!fs::exists(r.join("d1")).unwrap());
    assert_eq!(
        fs::rename(r.join("d2"), r.join("d2/x/inner")).unwrap_err().kind(),
        io::ErrorKind::InvalidInput
    );
    fs::create_dir(r.join("d3")).unwrap();
    fs::write(r.join("d3/keep"), b"").unwrap();
    assert_eq!(
        fs::rename(r.join("d2"), r.join("d3")).unwrap_err().kind(),
        io::ErrorKind::DirectoryNotEmpty
    );
    assert_eq!(
        fs::rename(r.join("nope"), r.join("x")).unwrap_err().kind(),
        io::ErrorKind::NotFound
    );
}

#[test]
fn copy_and_mtime_is_a_counter() {
    let r = root("copy");
    fs::write(r.join("a"), b"abc").unwrap();
    let t1 = fs::metadata(r.join("a")).unwrap().modified().unwrap();
    assert_eq!(fs::copy(r.join("a"), r.join("b")).unwrap(), 3);
    let t2 = fs::metadata(r.join("b")).unwrap().modified().unwrap();
    assert!(t2 > t1, "every mutation moves the clock");
    // a counter, not the wall clock: before 2100-01-01
    let secs = t2
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    assert!((1_577_836_800..4_102_444_800).contains(&secs));
    // reading does not touch
    fs::read(r.join("b")).unwrap();
    assert_eq!(fs::metadata(r.join("b")).unwrap().modified().unwrap(), t2);
}

#[test]
fn paths_are_normalised_lexically() {
    assert_eq!(lexical(Path::new("/a/./b/../c")), PathBuf::from("/a/c"));
    assert_eq!(lexical(Path::new("/../..")), PathBuf::from("/"));
    assert_eq!(lexical(Path::new("/a/b/")), PathBuf::from("/a/b"));
    assert!(normalize(Path::new("rel/x")).is_absolute());
    let r = root("canon");
    fs::create_dir_all(r.join("p/q")).unwrap();
    fs::write(r.join("p/q/f"), b"").unwrap();
    assert_eq!(
        fs::canonicalize(r.join("p/./q/../q/f")).unwrap(),
        r.join("p/q/f")
    );
    assert_eq!(fs::read(r.join("p/x/../q/f")).unwrap(), b"");
    assert_eq!(
        fs::canonicalize(r.join("p/none")).unwrap_err().kind(),
        io::ErrorKind::NotFound
    );
}

#[test]
fn implicit_directories() {
    let b = MemoryBackend::new();
    b.create_dir(Path::new("/x")).unwrap();
    b.open(
        Path::new("/x/f"),
        fs::OpenOptions::new().write(true).create(true),
    )
    .unwrap();
    // `/` is always a directory
    assert!(b.metadata(Path::new("/")).unwrap().is_dir());
    assert_eq!(b.read_dir(Path::new("/")).unwrap().len(), 1);
    // a backend is independent of the process-wide one
    assert!(!fs::exists("/x/f").unwrap());
    let backend: Arc<dyn Backend> = Arc::new(b);
    assert!(backend.metadata(Path::new("/x/f")).unwrap().is_file());
}

#[test]
fn walk_pre_order_depths_and_filter() {
    let r = root("walk");
    for f in ["b/x.sql", "b/y.sql", "a/deep/z.sql", "top.yml", "ignored/w.sql"] {
        let p = r.join(f);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, b"").unwrap();
    }
    let rel = |p: &Path| p.strip_prefix(&r).unwrap().to_string_lossy().into_owned();
    let all: Vec<(String, usize)> = WalkDir::new(&r)
        .into_iter()
        .map(|e| e.unwrap())
        .map(|e| (rel(e.path()), e.depth()))
        .collect();
    assert_eq!(
        all,
        [
            ("", 0),
            ("a", 1),
            ("a/deep", 2),
            ("a/deep/z.sql", 3),
            ("b", 1),
            ("b/x.sql", 2),
            ("b/y.sql", 2),
            ("ignored", 1),
            ("ignored/w.sql", 2),
            ("top.yml", 1)
        ]
        .map(|(a, b)| (a.to_string(), b))
    );
    let files: Vec<String> = WalkDir::new(&r)
        .min_depth(1)
        .max_depth(2)
        .follow_links(true)
        .sort_by(|a, b| b.file_name().cmp(a.file_name()))
        .into_iter()
        .filter_entry(|e| e.file_name() != "ignored")
        .map(|e| e.unwrap())
        .filter(|e| e.file_type().is_file())
        .map(|e| rel(e.path()))
        .collect();
    assert_eq!(files, ["top.yml", "b/y.sql", "b/x.sql"]);
    // a missing root is one error
    let mut it = WalkDir::new(r.join("none")).into_iter();
    let err = it.next().unwrap().unwrap_err();
    assert_eq!(
        io::Error::from(err).kind(),
        io::ErrorKind::NotFound
    );
    assert!(it.next().is_none());
}

#[test]
fn tokio_shim_round_trip() {
    use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
    let r = root("tokio");
    let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
    rt.block_on(async {
        crate::memtokio::create_dir_all(r.join("d")).await.unwrap();
        let mut f = crate::memtokio::File::create(r.join("d/f")).await.unwrap();
        f.write_all(b"hello world").await.unwrap();
        f.flush().await.unwrap();
        let mut f = crate::memtokio::File::open(r.join("d/f")).await.unwrap();
        f.seek(SeekFrom::Start(6)).await.unwrap();
        let mut s = String::new();
        f.read_to_string(&mut s).await.unwrap();
        assert_eq!(s, "world");
        let mut rd = crate::memtokio::read_dir(r.join("d")).await.unwrap();
        let e = rd.next_entry().await.unwrap().unwrap();
        assert_eq!(e.file_name(), "f");
        assert!(rd.next_entry().await.unwrap().is_none());
        assert!(crate::memtokio::try_exists(r.join("d/f")).await.unwrap());
    });
}

#[cfg(vfs_memory)]
#[test]
fn mount_and_snapshot() {
    use crate::PathExt;
    let r = PathBuf::from("/dbt-vfs-test/mount/project");
    crate::mount(
        &r,
        [
            (PathBuf::from("dbt_project.yml"), b"name: p".to_vec()),
            (PathBuf::from("models/a.sql"), b"select 1".to_vec()),
        ],
    )
    .unwrap();
    assert!(r.join("models").vfs_is_dir());
    assert!(r.join("models/a.sql").vfs_is_file());
    assert!(!r.join("models/b.sql").vfs_exists());
    fs::create_dir_all(r.join("target")).unwrap();
    fs::write(r.join("target/manifest.json"), b"{}").unwrap();
    let snap: Vec<String> = crate::snapshot(&r)
        .unwrap()
        .into_iter()
        .map(|(p, _)| p.to_string_lossy().into_owned())
        .collect();
    assert_eq!(snap, ["dbt_project.yml", "models/a.sql", "target/manifest.json"]);
    // and nothing reached the real disk
    assert!(!Path::new("/dbt-vfs-test").exists());
}
