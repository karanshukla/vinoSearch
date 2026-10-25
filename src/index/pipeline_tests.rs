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
    let types = code_on();
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

fn code_on() -> FileTypes {
    let mut formats = crate::config::Formats::default();
    formats.code.enabled = Some(true);
    FileTypes::new(&formats).unwrap()
}

fn names_of(conn: &Connection) -> Vec<String> {
    paths(conn)
        .iter()
        .map(|p| {
            Path::new(p)
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned()
        })
        .collect()
}

/// The kind and file name of `query`'s hits as `scope` sends it: code alone
/// with `code:`, everything else without.
fn found(conn: &Connection, query: &str) -> Vec<(Kind, String)> {
    let vectors = Vectors::load(conn).unwrap();
    let (text, kinds) = scope(query);
    let q = FakeEmbed::vector(text);
    let words = Words::find(conn, text);
    let mut out = Vec::new();
    for kind in kinds {
        for h in search(conn, &vectors, &q, words.as_ref(), *kind, 10).unwrap() {
            let name = Path::new(&h.path).file_name().unwrap();
            out.push((h.kind, name.to_string_lossy().into_owned()));
        }
    }
    out
}

#[test]
fn code_is_indexed_apart_and_found_only_with_the_code_prefix() {
    let t = Tree::new("code");
    t.write(".git/HEAD", "ref: refs/heads/main\n");
    t.write(".gitignore", "generated/\n*.local.py\n");
    t.write(
        "src/config.rs",
        "/// Parse the config file.\npub fn parse_config(path: &Path) -> Config {\n    todo!()\n}\n",
    );
    t.write("notes.md", "how to parse the config by hand\n");
    t.write("generated/schema.rs", "pub fn parse_config() {}\n");
    t.write("tweaks.local.py", "def parse_config(): pass\n");
    t.write(
        "third_party/ini/parse.c",
        "int parse_config(void) { return 0; }\n",
    );
    t.write("api.pb.go", "func ParseConfig() {}\n");
    t.write(
        "deploy.py",
        "KEY = \"\"\"-----BEGIN RSA PRIVATE KEY-----\nMIIEow\n\"\"\"\ndef parse_config(): pass\n",
    );
    t.write(
        "gen.go",
        "// Code generated by stringer. DO NOT EDIT.\npackage x\nfunc parse_config() {}\n",
    );
    let mut conn = t.db();
    let mut emb = FakeEmbed::default();
    let run_code = |conn: &mut Connection, emb: &mut FakeEmbed| {
        index(
            conn,
            emb,
            &t.root(),
            &Excludes::default(),
            &code_on(),
            &mut |_, _, _| Ok(()),
        )
        .unwrap()
    };
    let st = run_code(&mut conn, &mut emb);
    // config.rs and notes.md. deploy.py and gen.go are read, found
    // unwanted, and kept as rows with no chunks so they aren't read again.
    assert_eq!((st.files_seen, st.files_embedded), (4, 2));
    assert_eq!(emb.documents, 2);
    assert_eq!(
        names_of(&conn),
        ["deploy.py", "gen.go", "notes.md", "config.rs"]
    );
    assert_eq!(
        counts_by_kind(&conn).unwrap(),
        [(1, 1), (0, 0), (0, 0), (1, 1)]
    );
    // Code chunks carry their line, for opening an editor there.
    let line: i64 = conn
        .query_row("SELECT start_line FROM chunks WHERE kind = 3", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(line, 1);

    assert_eq!(
        found(&conn, "code: parse config"),
        [(Kind::Code, "config.rs".to_owned())]
    );
    assert_eq!(
        found(&conn, "parse config"),
        [(Kind::Text, "notes.md".to_owned())]
    );

    // Nothing is read again while unchanged.
    let st = run_code(&mut conn, &mut emb);
    assert_eq!(st.files_embedded, 0);
    assert_eq!(emb.documents, 2);

    // Ignored from when the .gitignore says so, at the next check.
    t.write(".gitignore", "generated/\n*.local.py\nsrc/\n");
    let stale = stale(&conn, &t.root(), &Excludes::default(), &code_on()).unwrap();
    assert_eq!(stale, [t.root().join("src/config.rs")]);

    // Turned off, code leaves the index.
    assert_eq!(purge_types(&conn, &FileTypes::default()).unwrap(), 3);
    assert_eq!(names_of(&conn), ["notes.md"]);
}

#[test]
fn code_indexed_as_text_moves_to_its_own_kind_once() {
    let t = Tree::new("code-migrate");
    t.write("a.rs", "fn alpha() {}\n");
    t.write("b.md", "beta notes\n");
    t.write("c.py", "def gamma(): pass\n");
    let mut conn = t.db();
    let mut emb = FakeEmbed::default();
    index(
        &mut conn,
        &mut emb,
        &t.root(),
        &Excludes::default(),
        &code_on(),
        &mut |_, _, _| Ok(()),
    )
    .unwrap();
    // As an index from before #110 has them: every chunk text, a file
    // holding a key among them.
    let key = t.root().join("key.py");
    conn.execute(
        "INSERT INTO files (path, mtime, size, sha256) VALUES (?1, 0, 0, x'01')",
        [key.to_str().unwrap()],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO chunks (file_id, start_line, text, embedding, kind)
         VALUES (last_insert_rowid(), 1, 'K = ''-----BEGIN EC PRIVATE KEY-----''', x'00', 0)",
        [],
    )
    .unwrap();
    conn.execute_batch("UPDATE chunks SET kind = 0; DELETE FROM meta WHERE key = 'kind.code';")
        .unwrap();
    let mut vectors = Vectors::load(&conn).unwrap();
    drop(conn);

    let conn = t.db();
    assert_eq!(names_of(&conn), ["a.rs", "b.md", "c.py"]);
    assert_eq!(
        counts_by_kind(&conn).unwrap(),
        [(1, 1), (0, 0), (0, 0), (2, 2)]
    );
    // Vectors loaded before catch the new kinds up from the log.
    vectors.refresh(&conn).unwrap();
    let q = FakeEmbed::vector("alpha");
    assert_eq!(
        search(&conn, &vectors, &q, None, Kind::Code, 5)
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        search(&conn, &vectors, &q, None, Kind::Text, 5)
            .unwrap()
            .len(),
        1
    );
    // Once only.
    conn.execute("UPDATE chunks SET kind = 0", []).unwrap();
    drop(conn);
    assert_eq!(counts_by_kind(&t.db()).unwrap()[3], (0, 0));
}
