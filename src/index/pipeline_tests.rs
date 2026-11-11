//! `index`, `index_file` and `index_rest` end to end on a temporary tree and
//! database, through `FakeEmbed`: what gets embedded, skipped, removed and
//! resumed. No OpenVINO or model.

use super::*;
use crate::ml::embed::FakeEmbed;
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
        [(1, 1), (0, 0), (0, 0), (1, 1), (0, 0), (0, 0)]
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
        [(1, 1), (0, 0), (0, 0), (2, 2), (0, 0), (0, 0)]
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

const SESSION_V1: &str = "vinowhisper-session/1";

fn sessions_on() -> FileTypes {
    FileTypes::default().with_sessions(true, None)
}

/// (transcript, summary) chunks of `path`.
fn session_parts(conn: &Connection, path: &Path) -> (i64, i64) {
    conn.query_row(
        "SELECT coalesce(sum(c.start_line < ?2), 0), coalesce(sum(c.start_line >= ?2), 0)
         FROM chunks c JOIN files f ON f.id = c.file_id WHERE f.path = ?1",
        params![
            path.to_str().unwrap(),
            crate::extract::session::SUMMARY_LINES as i64
        ],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .unwrap()
}

#[test]
fn a_session_is_one_file_of_two_parts_that_comes_and_goes_whole() {
    use crate::extract::session::tests::{SUMMARY_TEXT, bundle};
    let t = Tree::new("session");
    let path = t.root().join("2026-10-27-140512.vwsession");
    fs::write(&path, bundle(None, SESSION_V1)).unwrap();
    let mut conn = t.db();
    let mut emb = FakeEmbed::default();
    let types = sessions_on();
    index(
        &mut conn,
        &mut emb,
        &t.root(),
        &Excludes::default(),
        &types,
        &mut |_, _, _| Ok(()),
    )
    .unwrap();
    assert_eq!(session_parts(&conn, &path), (1, 0));
    assert_eq!(counts_by_kind(&conn).unwrap()[4], (1, 1));
    // Searched with everything else, not behind a prefix.
    assert!(
        found(&conn, "quarterly budget")
            .contains(&(Kind::Session, "2026-10-27-140512.vwsession".to_owned()))
    );

    // vinoWhisper adds the summary: a new bundle renamed over the old one,
    // which the watcher hands over as a changed path.
    let tmp = t.root().join(".2026-10-27-140512.vwsession.k3j.tmp");
    fs::write(&tmp, bundle(Some(SUMMARY_TEXT), SESSION_V1)).unwrap();
    fs::rename(&tmp, &path).unwrap();
    let out = index_file(&mut conn, &mut emb, &path, &types, &mut |_, _| Ok(())).unwrap();
    assert!(matches!(out, Outcome::Embedded(2)));
    assert_eq!(session_parts(&conn, &path), (1, 1));

    // Deleted, both parts go with it.
    fs::remove_file(&path).unwrap();
    index_file(&mut conn, &mut emb, &path, &types, &mut |_, _| Ok(())).unwrap();
    assert_eq!(session_parts(&conn, &path), (0, 0));
    assert!(paths(&conn).is_empty());
}

#[test]
fn a_session_of_an_unknown_format_is_kept_unread_and_counted() {
    use crate::extract::session::tests::{SUMMARY_TEXT, bundle};
    let t = Tree::new("session-v2");
    let path = t.root().join("new.vwsession");
    fs::write(&path, bundle(Some(SUMMARY_TEXT), "vinowhisper-session/2")).unwrap();
    t.write("old.vwsession", "not a zip at all");
    let mut conn = t.db();
    let mut emb = FakeEmbed::default();
    index(
        &mut conn,
        &mut emb,
        &t.root(),
        &Excludes::default(),
        &sessions_on(),
        &mut |_, _, _| Ok(()),
    )
    .unwrap();
    assert_eq!(emb.documents, 0);
    assert_eq!(names_of(&conn), ["new.vwsession", "old.vwsession"]);
    assert_eq!(unread_sessions(&conn).unwrap(), 2);
    // Turned off, sessions leave the index.
    assert_eq!(purge_types(&conn, &FileTypes::default()).unwrap(), 2);
}

#[test]
fn a_copy_of_a_session_is_not_unread() {
    use crate::extract::session::tests::bundle;
    let t = Tree::new("session-copy");
    let b = bundle(None, SESSION_V1);
    // As test fixtures sit in several repos.
    for name in ["a/s.vwsession", "b/s.vwsession", "c/s.vwsession"] {
        let p = t.root().join(name);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, &b).unwrap();
    }
    let mut conn = t.db();
    let mut emb = FakeEmbed::default();
    index(
        &mut conn,
        &mut emb,
        &t.root(),
        &Excludes::default(),
        &sessions_on(),
        &mut |_, _, _| Ok(()),
    )
    .unwrap();
    assert_eq!(paths(&conn).len(), 3);
    assert_eq!(unread_sessions(&conn).unwrap(), 0);
}

/// Media on, as if ffmpeg were installed, held to `max_seconds`.
fn media_on(max_seconds: f64) -> FileTypes {
    FileTypes::default().with_media(true, max_seconds)
}

/// 330 s of tiny video, red until 200 s then blue, under a tone silent from
/// 100 to 110 s. None without ffmpeg.
fn film(t: &Tree, name: &str) -> Option<PathBuf> {
    if !crate::extract::media::ffmpeg() {
        eprintln!("no ffmpeg; skipped");
        return None;
    }
    let path = t.root().join(name);
    let ok = std::process::Command::new("ffmpeg")
        .args(["-v", "error", "-y"])
        .args(["-f", "lavfi", "-i", "color=red:s=64x48:r=2:d=200"])
        .args(["-f", "lavfi", "-i", "color=blue:s=64x48:r=2:d=130"])
        .args(["-f", "lavfi", "-i", "sine=f=440:r=16000:d=330"])
        .args(["-filter_complex"])
        .arg("[0:v][1:v]concat=n=2:v=1:a=0[v];[2:a]volume='if(between(t,100,110),0,0.5)':eval=frame[a]")
        .args(["-map", "[v]", "-map", "[a]", "-c:v", "libx264", "-pix_fmt", "yuv420p"])
        .args(["-c:a", "aac"])
        .arg(&path)
        .status()
        .unwrap()
        .success();
    assert!(ok);
    Some(path)
}

/// (track, second) of every chunk of `path`, in order.
fn media_chunks(conn: &Connection, path: &Path) -> Vec<(crate::extract::media::Track, i64)> {
    conn.prepare(
        "SELECT c.start_line FROM chunks c JOIN files f ON f.id = c.file_id
         WHERE f.path = ?1 AND c.kind = 5 ORDER BY c.start_line",
    )
    .unwrap()
    .query_map([path.to_str().unwrap()], |r| r.get::<_, i64>(0))
    .unwrap()
    .map(|n| crate::extract::media::Track::of(n.unwrap()))
    .collect()
}

#[test]
fn media_goes_in_by_slices_from_index_rest() {
    use crate::extract::media::Track;
    let t = Tree::new("media");
    let Some(path) = film(&t, "walk.mp4") else {
        return;
    };
    let types = media_on(3600.0);
    let mut conn = t.db();
    let mut emb = FakeEmbed::default();
    let st = index(
        &mut conn,
        &mut emb,
        &t.root(),
        &Excludes::default(),
        &types,
        &mut |_, _, _| Ok(()),
    )
    .unwrap();
    // Recorded, nothing embedded yet; waits for AC.
    assert_eq!((st.files_embedded, st.chunks), (1, 0));
    assert_eq!(partial(&conn, 10).unwrap()[0].2, 330);
    assert!(!has_partial(&conn, false).unwrap());
    assert!(has_partial(&conn, true).unwrap());

    // A stop mid-slice keeps nothing of it.
    let mut n = 0;
    let stopped = index_rest(&mut conn, &mut emb, &types, &mut |_, _| {
        n += 1;
        if n == 3 { bail!("stop") } else { Ok(()) }
    });
    assert!(stopped.is_err());
    assert!(media_chunks(&conn, &path).is_empty());
    assert_eq!(partial(&conn, 10).unwrap()[0].1, 0);

    let first = index_rest(&mut conn, &mut emb, &types, &mut |_, _| Ok(()))
        .unwrap()
        .unwrap();
    assert!(
        matches!(
            first,
            Rest::Parts {
                done: 300,
                total: 330,
                ..
            }
        ),
        "{first:?}"
    );
    let rest = index_rest(&mut conn, &mut emb, &types, &mut |_, _| Ok(()))
        .unwrap()
        .unwrap();
    assert!(
        matches!(
            rest,
            Rest::Parts {
                done: 330,
                total: 330,
                ..
            }
        ),
        "{rest:?}"
    );
    assert!(!has_partial(&conn, true).unwrap());

    let chunks = media_chunks(&conn, &path);
    let sound: Vec<i64> = chunks
        .iter()
        .filter(|c| c.0 == Track::Sound)
        .map(|c| c.1)
        .collect();
    let picture: Vec<i64> = chunks
        .iter()
        .filter(|c| c.0 == Track::Picture)
        .map(|c| c.1)
        .collect();
    // Every 5 s but the one window that is all silence.
    let want: Vec<i64> = (0..65).map(|i| i * 5).filter(|&s| s != 100).collect();
    assert_eq!(sound, want);
    // The first frame, one per 30 s, the cut to blue, and the second
    // slice's first.
    assert_eq!(
        picture,
        [0, 30, 60, 90, 120, 150, 180, 200, 230, 260, 290, 300]
    );
    // Less the three windows the stopped run embedded and threw away.
    assert_eq!((emb.audio - 3, emb.images), (sound.len(), picture.len()));

    // Moved: the same chunks, nothing embedded again.
    let moved = t.root().join("trips/walk.mp4");
    fs::create_dir_all(moved.parent().unwrap()).unwrap();
    fs::rename(&path, &moved).unwrap();
    let embedded = (emb.audio, emb.images);
    index(
        &mut conn,
        &mut emb,
        &t.root(),
        &Excludes::default(),
        &types,
        &mut |_, _, _| Ok(()),
    )
    .unwrap();
    assert_eq!((emb.audio, emb.images), embedded);
    assert_eq!(media_chunks(&conn, &moved), chunks);
    assert!(!has_partial(&conn, true).unwrap());

    // Deleted: gone.
    fs::remove_file(&moved).unwrap();
    index(
        &mut conn,
        &mut emb,
        &t.root(),
        &Excludes::default(),
        &types,
        &mut |_, _, _| Ok(()),
    )
    .unwrap();
    assert_eq!(counts(&conn).unwrap(), (0, 0));
}

#[test]
fn media_waits_for_documents_and_long_files_are_left_out() {
    let t = Tree::new("media-order");
    let Some(path) = film(&t, "walk.mp4") else {
        return;
    };
    let line = format!("{}\n", "lorem ipsum dolor sit amet ".repeat(3));
    let book = t.write("book.txt", &line.repeat(1500));
    let mut conn = t.db();
    let mut emb = FakeEmbed::default();
    let types = media_on(3600.0);
    index(
        &mut conn,
        &mut emb,
        &t.root(),
        &Excludes::default(),
        &types,
        &mut |_, _, _| Ok(()),
    )
    .unwrap();
    // The book's id may come after the film's: taken first anyway.
    assert!(has_partial(&conn, false).unwrap());
    let next = index_rest(&mut conn, &mut emb, &types, &mut |_, _| Ok(()))
        .unwrap()
        .unwrap();
    assert!(matches!(next, Rest::Parts { ref path, .. } if path == book.to_str().unwrap()));

    // Over the longest allowed: a row, no chunks, no partial.
    let short = media_on(60.0);
    fs::remove_file(&book).unwrap();
    let mut conn = Tree::new("media-long").db();
    let st = index(
        &mut conn,
        &mut emb,
        &t.root(),
        &Excludes::default(),
        &short,
        &mut |_, _, _| Ok(()),
    )
    .unwrap();
    assert_eq!(st.files_embedded, 0);
    assert!(partial(&conn, 10).unwrap().is_empty());
    assert!(is_indexed(&conn, path.to_str().unwrap()).unwrap());
}

/// One check of `t`'s root under `types`.
fn check(conn: &mut Connection, emb: &mut FakeEmbed, t: &Tree, types: &FileTypes) -> Stats {
    index(
        conn,
        emb,
        &t.root(),
        &Excludes::default(),
        types,
        &mut |_, _, _| Ok(()),
    )
    .unwrap()
}

/// `seconds` of a tone as a WAV, whose length ffprobe gives exactly. None
/// without ffmpeg.
fn tone(t: &Tree, name: &str, hz: u32, seconds: u32) -> Option<PathBuf> {
    if !crate::extract::media::ffmpeg() {
        eprintln!("no ffmpeg; skipped");
        return None;
    }
    let path = t.root().join(name);
    let ok = std::process::Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-f", "lavfi", "-i"])
        .arg(format!("sine=f={hz}:r=16000:d={seconds}"))
        .arg(&path)
        .status()
        .unwrap()
        .success();
    assert!(ok);
    Some(path)
}

/// The next slice of whatever waits, which must not fail.
fn slice(conn: &mut Connection, emb: &mut FakeEmbed, types: &FileTypes) -> Option<Rest> {
    index_rest(conn, emb, types, &mut |_, _| Ok(())).unwrap()
}

#[test]
fn copies_of_a_recording_wait_as_one_and_either_can_go() {
    let t = Tree::new("media-copies");
    let Some(a) = tone(&t, "a.wav", 440, 330) else {
        return;
    };
    let b = t.root().join("b.wav");
    fs::copy(&a, &b).unwrap();
    let types = media_on(3600.0);
    let mut conn = t.db();
    let mut emb = FakeEmbed::default();
    let key = |p: &Path| p.to_str().unwrap().to_owned();

    // Both found before either is embedded: one waits, the other is its copy.
    let st = check(&mut conn, &mut emb, &t, &types);
    assert_eq!((st.files_embedded, st.duplicates), (1, 1));
    assert_eq!(partial(&conn, 10).unwrap().len(), 1);

    // The switch turned off drops both, and on again they are as before.
    assert_eq!(purge_types(&conn, &FileTypes::default()).unwrap(), 2);
    assert!(!has_partial(&conn, true).unwrap());
    let st = check(&mut conn, &mut emb, &t, &types);
    assert_eq!((st.files_embedded, st.duplicates), (1, 1));

    // The one waiting is deleted: its copy waits in its place.
    let first = PathBuf::from(&partial(&conn, 10).unwrap()[0].0);
    let second = if first == a { b.clone() } else { a.clone() };
    fs::remove_file(&first).unwrap();
    check(&mut conn, &mut emb, &t, &types);
    assert_eq!(partial(&conn, 10).unwrap(), [(key(&second), 0, 330)]);

    // Deleted after its first slice, with a copy again: the copy carries on
    // from there with the chunks.
    fs::copy(&second, &first).unwrap();
    assert_eq!(check(&mut conn, &mut emb, &t, &types).duplicates, 1);
    slice(&mut conn, &mut emb, &types).unwrap();
    let had = media_chunks(&conn, &second);
    assert_eq!(had.len(), 60);
    fs::remove_file(&second).unwrap();
    check(&mut conn, &mut emb, &t, &types);
    assert_eq!(partial(&conn, 10).unwrap(), [(key(&first), 300, 330)]);
    assert_eq!(media_chunks(&conn, &first), had);
    slice(&mut conn, &mut emb, &types).unwrap();
    assert!(!has_partial(&conn, true).unwrap());
    let whole = media_chunks(&conn, &first);
    assert_eq!(whole.len(), 65);
    assert_eq!(emb.audio, 65);

    // Both whole, one deleted: the other holds each chunk once. Then the
    // copy deleted: nothing changes hands.
    fs::copy(&first, &second).unwrap();
    assert_eq!(check(&mut conn, &mut emb, &t, &types).duplicates, 1);
    fs::remove_file(&first).unwrap();
    check(&mut conn, &mut emb, &t, &types);
    assert_eq!(media_chunks(&conn, &second), whole);
    fs::copy(&second, &first).unwrap();
    check(&mut conn, &mut emb, &t, &types);
    fs::remove_file(&first).unwrap();
    check(&mut conn, &mut emb, &t, &types);
    assert_eq!(media_chunks(&conn, &second), whole);
    assert_eq!(emb.audio, 65);
    assert_eq!(counts(&conn).unwrap(), (1, 65));
}

#[test]
fn a_copy_takes_over_when_the_recording_it_waits_behind_changes() {
    let t = Tree::new("media-copy-changes");
    let Some(a) = tone(&t, "a.wav", 440, 20) else {
        return;
    };
    let b = t.root().join("b.wav");
    fs::copy(&a, &b).unwrap();
    let types = media_on(3600.0);
    let mut conn = t.db();
    let mut emb = FakeEmbed::default();
    check(&mut conn, &mut emb, &t, &types);
    let first = PathBuf::from(&partial(&conn, 10).unwrap()[0].0);
    let second = if first == a { b } else { a };
    // Another recording under the first one's name.
    tone(&t, first.file_name().unwrap().to_str().unwrap(), 880, 30).unwrap();
    let st = check(&mut conn, &mut emb, &t, &types);
    assert_eq!((st.files_embedded, st.duplicates), (1, 0));
    let mut waiting = partial(&conn, 10).unwrap();
    waiting.sort();
    let mut want = [
        (first.to_str().unwrap().to_owned(), 0, 30),
        (second.to_str().unwrap().to_owned(), 0, 20),
    ];
    want.sort();
    assert_eq!(waiting, want);
}

#[test]
fn an_index_where_two_copies_both_waited_is_put_right() {
    let t = Tree::new("media-two-rows");
    let conn = t.db();
    // As it was before a file waiting in `partial` counted as holding its
    // content: the two triggers, and two copies each with a row and chunks.
    conn.execute_batch(
        "DROP TRIGGER hand_over;
         CREATE TRIGGER files_hand_over BEFORE DELETE ON files
         WHEN OLD.sha256 IS NOT NULL
         BEGIN
             UPDATE chunks
             SET file_id = (SELECT id FROM files WHERE sha256 = OLD.sha256 AND id != OLD.id LIMIT 1)
             WHERE file_id = OLD.id
               AND EXISTS (SELECT 1 FROM files WHERE sha256 = OLD.sha256 AND id != OLD.id);
         END;
         CREATE TRIGGER partial_hand_over BEFORE DELETE ON files
         WHEN OLD.sha256 IS NOT NULL
         BEGIN
             UPDATE partial
             SET file_id = (SELECT id FROM files WHERE sha256 = OLD.sha256 AND id != OLD.id LIMIT 1)
             WHERE file_id = OLD.id
               AND EXISTS (SELECT 1 FROM files WHERE sha256 = OLD.sha256 AND id != OLD.id);
         END;
         INSERT INTO files (id, path, mtime, size, sha256) VALUES
             (1, '/v/a.mp4', 1, 1, x'aa'), (2, '/v/b.mp4', 1, 1, x'aa'), (3, '/v/c.mp4', 1, 1, x'aa');
         INSERT INTO partial (file_id, done, total, media) VALUES (1, 300, 900, 1), (2, 300, 900, 1);",
    )
    .unwrap();
    let chunks = [(0, String::new()), (5, String::new())];
    let vectors = vec![FakeEmbed::vector("sound"); 2];
    for id in [1, 2] {
        insert_chunks(&conn, id, &chunks, &vectors, &[Kind::Media; 2]).unwrap();
    }
    let unique = conn
        .execute("DELETE FROM files WHERE id = 1", [])
        .unwrap_err();
    assert!(unique.to_string().contains("partial.file_id"), "{unique}");
    drop(conn);

    // Opened by this build: the first keeps what it has, the second is its
    // copy like the third.
    let conn = t.db();
    let rows = |sql: &str| -> Vec<i64> {
        conn.prepare(sql)
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    };
    assert_eq!(rows("SELECT file_id FROM chunks"), [1, 1]);
    assert_eq!(rows("SELECT file_id FROM partial"), [1]);
    assert_eq!(
        rows(
            "SELECT count(*) FROM sqlite_master WHERE type = 'trigger' AND name LIKE '%hand_over'"
        ),
        [1]
    );
    // And either can go: one copy takes the chunks and the row, once.
    conn.execute("DELETE FROM files WHERE id = 1", []).unwrap();
    assert_eq!(rows("SELECT file_id FROM chunks"), [2, 2]);
    assert_eq!(rows("SELECT file_id FROM partial"), [2]);
    conn.execute("DELETE FROM files WHERE id = 3", []).unwrap();
    assert_eq!(rows("SELECT file_id FROM chunks"), [2, 2]);

    // Should two ever each hold the content again, deleting one leaves the
    // other as it is: not a second row, nor every chunk twice.
    conn.execute_batch(
        "INSERT INTO files (id, path, mtime, size, sha256) VALUES (4, '/v/d.mp4', 1, 1, x'aa');
         INSERT INTO partial (file_id, done, total, media) VALUES (4, 0, 900, 1);",
    )
    .unwrap();
    insert_chunks(&conn, 4, &chunks, &vectors, &[Kind::Media; 2]).unwrap();
    conn.execute("DELETE FROM files WHERE id = 4", []).unwrap();
    assert_eq!(rows("SELECT file_id FROM chunks"), [2, 2]);
    assert_eq!(rows("SELECT done FROM partial"), [300]);
}

/// A 12 s float WAV of a tone in which some samples are NaN.
fn broken_wav(t: &Tree, name: &str) -> PathBuf {
    let mut data = Vec::new();
    for i in 0..16_000 * 12 {
        let v = if i % 1000 == 7 {
            f32::NAN
        } else {
            0.3 * (i as f32 * 440.0 * std::f32::consts::TAU / 16_000.0).sin()
        };
        data.extend(v.to_le_bytes());
    }
    let mut wav = Vec::new();
    wav.extend(b"RIFF");
    wav.extend((36 + data.len() as u32).to_le_bytes());
    wav.extend(b"WAVEfmt ");
    // 16 bytes of format: IEEE float, mono, 16 kHz, 32 bits.
    wav.extend(16u32.to_le_bytes());
    wav.extend(3u16.to_le_bytes());
    wav.extend(1u16.to_le_bytes());
    wav.extend(16_000u32.to_le_bytes());
    wav.extend(64_000u32.to_le_bytes());
    wav.extend(4u16.to_le_bytes());
    wav.extend(32u16.to_le_bytes());
    wav.extend(b"data");
    wav.extend((data.len() as u32).to_le_bytes());
    wav.extend(data);
    let path = t.root().join(name);
    fs::write(&path, wav).unwrap();
    path
}

#[test]
fn sound_that_isnt_numbers_stops_its_file_and_not_the_ones_after() {
    let t = Tree::new("media-nan");
    if !crate::extract::media::ffmpeg() {
        return;
    }
    let broken = broken_wav(&t, "broken.wav");
    let types = media_on(3600.0);
    let mut conn = t.db();
    let mut emb = FakeEmbed::default();
    check(&mut conn, &mut emb, &t, &types);
    // Found first, so first in line.
    let fine = tone(&t, "fine.wav", 440, 12).unwrap();
    check(&mut conn, &mut emb, &t, &types);
    assert_eq!(partial(&conn, 10).unwrap().len(), 2);

    let stopped = slice(&mut conn, &mut emb, &types).unwrap();
    assert!(
        matches!(stopped, Rest::Parts { ref path, done: 0, chunks: 0, .. } if path == broken.to_str().unwrap()),
        "{stopped:?}"
    );
    assert_eq!(emb.audio, 0);
    let next = slice(&mut conn, &mut emb, &types).unwrap();
    assert!(
        matches!(next, Rest::Parts { ref path, done: 12, chunks: 2, .. } if path == fine.to_str().unwrap()),
        "{next:?}"
    );
    assert!(slice(&mut conn, &mut emb, &types).is_none());
    // Not tried again until it changes.
    assert_eq!(check(&mut conn, &mut emb, &t, &types).files_embedded, 0);
    assert!(!has_partial(&conn, true).unwrap());
}

#[test]
fn a_recording_the_model_fails_on_goes_behind_the_others() {
    let t = Tree::new("media-model-fails");
    let Some(a) = tone(&t, "a.wav", 440, 12) else {
        return;
    };
    let types = media_on(3600.0);
    let mut conn = t.db();
    let mut emb = FakeEmbed::default();
    check(&mut conn, &mut emb, &t, &types);
    let b = tone(&t, "b.wav", 880, 12).unwrap();
    check(&mut conn, &mut emb, &t, &types);
    let line = |conn: &Connection| -> Vec<String> {
        let waiting = partial(conn, 10).unwrap();
        waiting.into_iter().map(|w| w.0).collect()
    };
    let (a, b) = (a.to_str().unwrap(), b.to_str().unwrap());
    assert_eq!(line(&conn), [a, b]);

    emb.audio_fails = true;
    let failed = index_rest(&mut conn, &mut emb, &types, &mut |_, _| Ok(()));
    assert!(format!("{:#}", failed.unwrap_err()).contains("degenerate"));
    // Still waiting, with nothing of it kept, but no longer first.
    assert_eq!(line(&conn), [b, a]);
    emb.audio_fails = false;
    let next = slice(&mut conn, &mut emb, &types).unwrap();
    assert!(matches!(next, Rest::Parts { ref path, done: 12, .. } if path == b));
    let last = slice(&mut conn, &mut emb, &types).unwrap();
    assert!(matches!(last, Rest::Parts { ref path, done: 12, .. } if path == a));
    assert!(!has_partial(&conn, true).unwrap());

    // A stop is not the model failing: the file keeps its place.
    let c = tone(&t, "c.wav", 660, 12).unwrap();
    let d = tone(&t, "d.wav", 990, 12).unwrap();
    check(&mut conn, &mut emb, &t, &types);
    let before = line(&conn);
    assert_eq!(before.len(), 2, "{c:?} {d:?}");
    assert!(index_rest(&mut conn, &mut emb, &types, &mut |_, _| bail!("stop")).is_err());
    assert_eq!(line(&conn), before);
}

#[test]
fn media_waits_while_its_models_are_not_fetched() {
    let t = Tree::new("media-no-models");
    let Some(_) = tone(&t, "a.wav", 440, 12) else {
        return;
    };
    let line = format!("{}\n", "lorem ipsum dolor sit amet ".repeat(3));
    let book = t.write("book.txt", &line.repeat(1500));
    let types = media_on(3600.0);
    let mut conn = t.db();
    let mut emb = FakeEmbed {
        no_media_models: true,
        ..FakeEmbed::default()
    };
    check(&mut conn, &mut emb, &t, &types);
    // The book's rest goes in; the recording is neither decoded nor failed.
    while let Some(rest) = slice(&mut conn, &mut emb, &types) {
        assert!(matches!(rest, Rest::Parts { ref path, .. } if path == book.to_str().unwrap()));
    }
    assert!(!has_partial(&conn, false).unwrap());
    assert!(has_partial(&conn, true).unwrap());
    assert_eq!(emb.audio, 0);
    // Fetched: taken up.
    emb.no_media_models = false;
    assert!(slice(&mut conn, &mut emb, &types).is_some());
    assert_eq!(emb.audio, 2);
}

#[test]
fn a_file_found_with_neither_track_stops_instead_of_panicking() {
    let t = Tree::new("media-subtitles");
    if !crate::extract::media::ffmpeg() {
        return;
    }
    // Subtitles only, behind a partial row: a film replaced under the same
    // stamp since it was recorded.
    let srt = t.0.join("lines.srt");
    fs::write(
        &srt,
        "1\n00:00:01,000 --> 00:00:04,000\nhello\n\n2\n00:00:20,000 --> 00:00:24,000\nbye\n",
    )
    .unwrap();
    let path = t.root().join("subtitles.mkv");
    let ok = std::process::Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-i"])
        .arg(&srt)
        .args(["-c:s", "srt"])
        .arg(&path)
        .status()
        .unwrap()
        .success();
    assert!(ok);
    let types = media_on(3600.0);
    let mut conn = t.db();
    let mut emb = FakeEmbed::default();
    // Found now, it is recorded with nothing to embed.
    let st = check(&mut conn, &mut emb, &t, &types);
    assert_eq!(st.files_embedded, 0);
    assert!(!has_partial(&conn, true).unwrap());
    conn.execute(
        "INSERT INTO partial (file_id, done, total, media)
         SELECT id, 0, 24, 1 FROM files WHERE path = ?1",
        [path.to_str().unwrap()],
    )
    .unwrap();
    let stopped = slice(&mut conn, &mut emb, &types).unwrap();
    assert!(
        matches!(
            stopped,
            Rest::Parts {
                done: 0,
                chunks: 0,
                ..
            }
        ),
        "{stopped:?}"
    );
    assert!(!has_partial(&conn, true).unwrap());
}

#[test]
fn the_longest_media_indexed_follows_the_setting() {
    let t = Tree::new("media-length");
    let Some(path) = tone(&t, "talk.wav", 440, 40) else {
        return;
    };
    let key = path.to_str().unwrap();
    let mut conn = t.db();
    let mut emb = FakeEmbed::default();
    let hashed = |conn: &Connection| -> bool {
        conn.query_row(
            "SELECT sha256 IS NOT NULL FROM files WHERE path = ?1",
            [key],
            |r| r.get(0),
        )
        .unwrap()
    };

    // Over the longest allowed: a row, and not so much as read for its hash.
    let short = media_on(30.0);
    let st = check(&mut conn, &mut emb, &t, &short);
    assert_eq!((st.files_seen, st.files_embedded), (1, 0));
    assert!(!hashed(&conn));
    assert!(!has_partial(&conn, true).unwrap());
    // Seen again, it is left alone; so it is when the limit moves but not
    // past it.
    assert_eq!(check(&mut conn, &mut emb, &t, &short).files_embedded, 0);
    purge_types(&conn, &media_on(35.0)).unwrap();
    assert!(!has_partial(&conn, true).unwrap());

    // Raised past it: taken up without waiting for the file to change.
    let long = media_on(60.0);
    assert_eq!(purge_types(&conn, &long).unwrap(), 0);
    assert!(has_partial(&conn, true).unwrap());
    let again = slice(&mut conn, &mut emb, &long).unwrap();
    assert!(matches!(again, Rest::Again(_)), "{again:?}");
    assert!(hashed(&conn));
    let whole = slice(&mut conn, &mut emb, &long).unwrap();
    assert!(matches!(whole, Rest::Parts { done: 40, .. }), "{whole:?}");
    assert_eq!(media_chunks(&conn, &path).len(), 7);

    // Lowered under it: its chunks go, its row stays, and a check finds
    // nothing to do.
    purge_types(&conn, &short).unwrap();
    assert!(media_chunks(&conn, &path).is_empty());
    assert!(is_indexed(&conn, key).unwrap());
    let st = check(&mut conn, &mut emb, &t, &short);
    assert_eq!((st.files_seen, st.files_embedded), (1, 0));
    assert!(!has_partial(&conn, true).unwrap());

    // And raised again: queued as it is, hashed already.
    let heard = emb.audio;
    purge_types(&conn, &long).unwrap();
    let whole = slice(&mut conn, &mut emb, &long).unwrap();
    assert!(matches!(whole, Rest::Parts { done: 40, .. }), "{whole:?}");
    assert_eq!(media_chunks(&conn, &path).len(), 7);
    assert_eq!(emb.audio, heard + 7);

    // A copy of one that fits is its copy, not a second run.
    let copy = t.root().join("copy.wav");
    fs::copy(&path, &copy).unwrap();
    assert_eq!(check(&mut conn, &mut emb, &t, &long).duplicates, 1);
    purge_types(&conn, &short).unwrap();
    purge_types(&conn, &long).unwrap();
    assert_eq!(partial(&conn, 10).unwrap().len(), 1);
}
