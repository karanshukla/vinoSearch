//! `index`, `index_file` and `index_rest` end to end on a temporary tree and
//! database, through `FakeEmbed`: what gets embedded, skipped, removed and
//! resumed. No OpenVINO or model.

use super::*;
use crate::embed::FakeEmbed;
use std::os::unix::fs::symlink;
use std::time::{Duration, SystemTime};

/// A directory of its own under the temp dir, removed when dropped.
struct Tree(PathBuf);

impl Tree {
    fn new(name: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("vinosearch-pipeline-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("root")).unwrap();
        Tree(fs::canonicalize(&dir).unwrap())
    }

    fn root(&self) -> PathBuf {
        self.0.join("root")
    }

    fn write(&self, name: &str, text: &str) -> PathBuf {
        let p = self.root().join(name);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, text).unwrap();
        p
    }

    fn db(&self) -> Connection {
        open(&self.0.join("index.sqlite")).unwrap()
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn run(conn: &mut Connection, emb: &mut FakeEmbed, root: &Path) -> Stats {
    index(
        conn,
        emb,
        root,
        &Excludes::default(),
        &FileTypes::default(),
        &mut |_, _, _| Ok(()),
    )
    .unwrap()
}

fn paths(conn: &Connection) -> Vec<String> {
    conn.prepare("SELECT path FROM files ORDER BY path")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

fn top(conn: &Connection, query: &str) -> String {
    let vectors = Vectors::load(conn).unwrap();
    let q = FakeEmbed::vector(query);
    search(conn, &vectors, &q, None, Kind::Text, 1).unwrap()[0]
        .path
        .clone()
}

#[test]
fn an_indexed_tree_is_searchable() {
    let t = Tree::new("search");
    let fruit = t.write("fruit.md", "apples and pears\n");
    let bread = t.write("notes/bread.txt", "banana bread with walnuts\n");
    let mut conn = t.db();
    let mut emb = FakeEmbed::default();
    let st = run(&mut conn, &mut emb, &t.root());
    assert_eq!((st.files_seen, st.files_embedded, st.chunks), (2, 2, 2));
    assert_eq!(emb.documents, 2);
    assert_eq!(roots(&conn).unwrap(), [t.root()]);
    assert_eq!(top(&conn, "banana walnuts"), bread.to_str().unwrap());
    assert_eq!(top(&conn, "pears"), fruit.to_str().unwrap());
}

#[test]
fn unchanged_and_touched_files_are_not_embedded_again() {
    let t = Tree::new("unchanged");
    let a = t.write("a.md", "first draft\n");
    t.write("b.md", "something else\n");
    let mut conn = t.db();
    let mut emb = FakeEmbed::default();
    run(&mut conn, &mut emb, &t.root());
    assert_eq!(emb.documents, 2);

    let st = run(&mut conn, &mut emb, &t.root());
    assert_eq!((st.files_seen, st.files_embedded), (2, 0));
    assert_eq!(emb.documents, 2);

    // A new mtime with the same content: the hash says nothing changed.
    let later = SystemTime::now() + Duration::from_secs(3600);
    fs::File::options()
        .write(true)
        .open(&a)
        .unwrap()
        .set_modified(later)
        .unwrap();
    let st = run(&mut conn, &mut emb, &t.root());
    assert_eq!(st.files_embedded, 0);
    assert_eq!(emb.documents, 2);

    fs::write(&a, "second draft, longer\n").unwrap();
    let st = run(&mut conn, &mut emb, &t.root());
    assert_eq!(st.files_embedded, 1);
    assert_eq!(emb.documents, 3);
    assert_eq!(top(&conn, "second draft"), a.to_str().unwrap());
}

#[test]
fn a_copy_shares_the_chunks_of_the_file_it_copies() {
    let t = Tree::new("copy");
    t.write("a.md", "the same words\n");
    t.write("b.md", "the same words\n");
    let mut conn = t.db();
    let mut emb = FakeEmbed::default();
    let st = run(&mut conn, &mut emb, &t.root());
    assert_eq!((st.files_embedded, st.duplicates), (1, 1));
    assert_eq!(emb.documents, 1);
    assert_eq!(counts(&conn).unwrap(), (2, 1));
}

#[test]
fn files_gone_from_the_tree_leave_the_index() {
    let t = Tree::new("gone");
    t.write("keep.md", "keep me\n");
    let gone = t.write("sub/gone.md", "remove me\n");
    let mut conn = t.db();
    let mut emb = FakeEmbed::default();
    run(&mut conn, &mut emb, &t.root());
    assert_eq!(paths(&conn).len(), 2);

    fs::remove_file(&gone).unwrap();
    let st = run(&mut conn, &mut emb, &t.root());
    assert_eq!(st.removed, 1);
    assert_eq!(paths(&conn), [t.root().join("keep.md").to_str().unwrap()]);
    assert_eq!(counts(&conn).unwrap(), (1, 1));
}

#[test]
fn symlinks_oversize_and_unwanted_files_are_skipped() {
    let t = Tree::new("skip");
    let outside = t.0.join("outside.md");
    fs::write(&outside, "secret outside the root\n").unwrap();
    let link = t.root().join("link.md");
    symlink(&outside, &link).unwrap();
    let big = t.write("big.rs", &"x".repeat(MAX_FILE_BYTES as usize + 1));
    t.write("photo.xcf", "not a type we index\n");
    t.write(".hidden/a.md", "hidden directory\n");
    t.write("ok.md", "indexed\n");
    let mut conn = t.db();
    let mut emb = FakeEmbed::default();
    let types = FileTypes::default();
    let mut between = |_: &Connection, _: &mut FakeEmbed| Ok(());

    for p in [&link, &big] {
        let out = index_file(&mut conn, &mut emb, p, &types, &mut between).unwrap();
        assert!(matches!(out, Outcome::Skipped), "{}", p.display());
    }
    run(&mut conn, &mut emb, &t.root());
    assert_eq!(paths(&conn), [t.root().join("ok.md").to_str().unwrap()]);
    assert_eq!(emb.documents, 1);
}

#[test]
fn a_stop_mid_file_writes_nothing_of_it_and_the_next_run_finishes() {
    let t = Tree::new("stop");
    t.write("a.md", "alpha\n");
    t.write("b.md", "beta\n");
    let mut conn = t.db();
    let mut emb = FakeEmbed::default();
    let stopped = index(
        &mut conn,
        &mut emb,
        &t.root(),
        &Excludes::default(),
        &FileTypes::default(),
        &mut |_, _, _| bail!("stop"),
    );
    assert_eq!(
        stopped.err().map(|e| e.to_string()).as_deref(),
        Some("stop")
    );
    // The chunk was embedded, but its file never written.
    assert_eq!(emb.documents, 1);
    assert_eq!(counts(&conn).unwrap(), (0, 0));

    let st = run(&mut conn, &mut emb, &t.root());
    assert_eq!(st.files_embedded, 2);
    assert_eq!(counts(&conn).unwrap(), (2, 2));
}

#[test]
fn a_long_text_goes_in_by_runs_of_parts() {
    let t = Tree::new("long");
    let line = format!("{}\n", "lorem ipsum dolor sit amet ".repeat(3));
    let book = t.write("book.txt", &line.repeat(1500));
    let mut conn = t.db();
    let mut emb = FakeEmbed::default();
    let types = FileTypes::default();

    let st = run(&mut conn, &mut emb, &t.root());
    assert_eq!(st.chunks, PARTS_AT_ONCE * crate::extract::PART_CHUNKS);
    let [(path, done, total)] = &partial(&conn, 10).unwrap()[..] else {
        panic!("one partial file expected");
    };
    assert_eq!(path, book.to_str().unwrap());
    assert_eq!(*done as usize, PARTS_AT_ONCE);
    assert!(*total as usize > PARTS_AT_ONCE);

    let mut runs = 0;
    while let Some(rest) = index_rest(&mut conn, &mut emb, &types, &mut |_, _| Ok(())).unwrap() {
        assert!(matches!(rest, Rest::Parts { .. }));
        runs += 1;
        assert!(runs < 10);
    }
    assert!(partial(&conn, 10).unwrap().is_empty());
    let all = chunk(&fs::read_to_string(&book).unwrap()).len();
    assert_eq!(counts(&conn).unwrap(), (1, all as i64));
    assert_eq!(emb.documents, all);
}

#[test]
fn a_long_text_changed_between_runs_is_indexed_afresh() {
    let t = Tree::new("long-changed");
    let line = format!("{}\n", "lorem ipsum dolor sit amet ".repeat(3));
    let book = t.write("book.txt", &line.repeat(1500));
    let mut conn = t.db();
    let mut emb = FakeEmbed::default();
    run(&mut conn, &mut emb, &t.root());
    assert_eq!(partial(&conn, 10).unwrap().len(), 1);

    fs::write(&book, "short now\n").unwrap();
    let rest = index_rest(&mut conn, &mut emb, &FileTypes::default(), &mut |_, _| {
        Ok(())
    })
    .unwrap()
    .unwrap();
    assert!(matches!(rest, Rest::Again(ref p) if p == book.to_str().unwrap()));
    assert!(partial(&conn, 10).unwrap().is_empty());
    assert_eq!(counts(&conn).unwrap(), (1, 1));
    assert_eq!(top(&conn, "short"), book.to_str().unwrap());
}
