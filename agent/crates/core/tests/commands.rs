//! Stage 6.5: `.iris/commands/*.md` → prompt templates.
//!
//! `load` is best-effort — broken files become warnings, never panics
//! or blocked runs; `expand` is exact — wrong arity is an error, not a
//! guess.

use iris_core::commands::{ProjectCommand, load};
use std::fs;
use std::path::Path;

/// Drop a file into `<root>/.iris/commands/`.
fn write_command(root: &Path, name: &str, contents: &str) {
    let dir = root.join(".iris").join("commands");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join(name), contents).unwrap();
}

#[test]
fn load_discovers_markdown_commands_with_frontmatter() {
    let dir = tempfile::tempdir().unwrap();
    write_command(
        dir.path(),
        "review.md",
        "---\ndescription: Review a diff\nargs: [scope]\n---\nReview the {{scope}} diff.\n",
    );
    write_command(dir.path(), "explain.md", "Explain this file.\n");
    let (commands, warnings) = load(dir.path(), &["help", "quit", "clear"]);
    assert_eq!(warnings, Vec::<String>::new());
    assert_eq!(
        commands.keys().map(String::as_str).collect::<Vec<_>>(),
        vec!["explain", "review"],
        "sorted by name — /help lists them deterministically"
    );
    let review = &commands["review"];
    assert_eq!(review.description, "Review a diff");
    assert_eq!(review.args, vec!["scope".to_string()]);
    assert_eq!(review.body, "Review the {{scope}} diff.");
}

#[test]
fn a_file_without_frontmatter_is_a_body_only_command() {
    let dir = tempfile::tempdir().unwrap();
    write_command(dir.path(), "ping.md", "Just say pong.\n\n");
    let (commands, warnings) = load(dir.path(), &[]);
    assert_eq!(warnings, Vec::<String>::new());
    let ping = &commands["ping"];
    assert_eq!(ping.description, "");
    assert_eq!(ping.args, Vec::<String>::new());
    assert_eq!(ping.body, "Just say pong.");
}

#[test]
fn broken_files_warn_and_never_block_the_rest() {
    let dir = tempfile::tempdir().unwrap();
    write_command(dir.path(), "good.md", "Fine.\n");
    write_command(
        dir.path(),
        "typo.md",
        "---\ndescriptoin: oops\n---\nBody.\n",
    );
    write_command(dir.path(), "half.md", "---\ndescription: no close\nBody.\n");
    write_command(
        dir.path(),
        "empty.md",
        "---\ndescription: nothing\n---\n   \n",
    );
    let (commands, warnings) = load(dir.path(), &[]);
    assert!(commands.contains_key("good"), "the rest still load");
    assert_eq!(warnings.len(), 3, "{warnings:?}");
    let typo = warnings
        .iter()
        .find(|warning| warning.contains("typo"))
        .expect("the typo file is warned");
    assert!(
        typo.contains("unknown frontmatter key `descriptoin`"),
        "a typo'd key fails loudly: {typo}"
    );
    let half = warnings
        .iter()
        .find(|warning| warning.contains("half"))
        .expect("the unterminated file is warned");
    assert!(half.contains("unterminated"), "{half}");
    let empty = warnings
        .iter()
        .find(|warning| warning.contains("empty"))
        .expect("the empty-body file is warned");
    assert!(empty.contains("empty command body"), "{empty}");
}

#[test]
fn args_must_be_an_inline_list() {
    let dir = tempfile::tempdir().unwrap();
    write_command(dir.path(), "bad.md", "---\nargs: scope\n---\nBody.\n");
    let (commands, warnings) = load(dir.path(), &[]);
    assert!(commands.is_empty());
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("inline list"), "{}", warnings[0]);
}

#[test]
fn builtin_names_are_reserved() {
    let dir = tempfile::tempdir().unwrap();
    write_command(dir.path(), "help.md", "Shadow the builtin.\n");
    let (commands, warnings) = load(dir.path(), &["help", "quit", "clear"]);
    assert!(commands.is_empty(), "the builtin wins");
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(
        warnings[0].contains("/help.md shadows a builtin"),
        "{}",
        warnings[0]
    );
}

#[test]
fn load_without_a_commands_directory_is_empty() {
    let dir = tempfile::tempdir().unwrap();
    let (commands, warnings) = load(dir.path(), &[]);
    assert!(commands.is_empty());
    assert_eq!(
        warnings,
        Vec::<String>::new(),
        "a missing dir is not an error"
    );
}

#[test]
fn non_markdown_files_are_ignored() {
    let dir = tempfile::tempdir().unwrap();
    write_command(
        dir.path(),
        "notes.txt",
        "---\ndescription: no\n---\nNot a command.\n",
    );
    let (commands, warnings) = load(dir.path(), &[]);
    assert!(commands.is_empty());
    assert_eq!(warnings, Vec::<String>::new());
}

#[test]
fn expand_binds_argv_positionally() {
    let command = ProjectCommand {
        description: "Review a diff".into(),
        args: vec!["scope".into(), "style".into()],
        body: "{{style}} review of {{scope}}: focus on {{scope}}. Literal {{x}} stays.".into(),
    };
    let argv = vec!["backend".to_string(), "quick".to_string()];
    assert_eq!(
        iris_core::commands::expand(&command, &argv).unwrap(),
        "quick review of backend: focus on backend. Literal {{x}} stays.",
        "repeated placeholders all bind; undeclared {{x}} stays literal"
    );
}

#[test]
fn expand_rejects_a_wrong_argument_count() {
    let command = ProjectCommand {
        description: "Review a diff".into(),
        args: vec!["scope".into()],
        body: "Review {{scope}}.".into(),
    };
    let err = iris_core::commands::expand(&command, &[]).unwrap_err();
    assert_eq!(err.expected, 1);
    assert_eq!(err.got, 0, "too few is an error, not an empty substitution");
    let extra = vec!["a".to_string(), "b".to_string()];
    let err = iris_core::commands::expand(&command, &extra).unwrap_err();
    assert_eq!(err.got, 2, "too many is an error, not silently ignored");
}

// --- coverage fills (stage 7.2): every way a file becomes a warning -------

/// One broken file → exactly its warning, never a panic.
fn only_warning(contents: &str) -> String {
    let dir = tempfile::tempdir().unwrap();
    write_command(dir.path(), "broken.md", contents);
    let (commands, warnings) = load(dir.path(), &[]);
    assert!(commands.is_empty(), "the broken file never loads");
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    warnings.into_iter().next().unwrap()
}

#[test]
fn every_parse_error_surfaces_as_its_own_warning() {
    let cases: &[(&str, &str)] = &[
        ("---", "unterminated frontmatter"),
        ("---\nnonsense\n---\nBody.\n", "frontmatter lines must be"),
        (
            "---\ndescription: a\ndescription: b\n---\nB\n",
            "duplicate frontmatter key `description`",
        ),
        (
            "---\ndescription: a\nargs: [x]\nargs: [y]\n---\nB\n",
            "duplicate frontmatter key `args`",
        ),
        ("   \n", "empty command body"),
        (
            "---\ndescription: x\nargs: [a,,b]\n---\nB\n",
            "empty argument name",
        ),
        (
            "---\ndescription: x\nargs: [bad name]\n---\nB\n",
            "invalid argument name",
        ),
        (
            "---\ndescription: x\nargs: [s, s]\n---\nB\n",
            "duplicate argument",
        ),
    ];
    for (contents, expect) in cases {
        let warning = only_warning(contents);
        assert!(warning.contains(expect), "{contents:?} → {warning}");
    }
}

#[test]
fn blank_frontmatter_lines_and_an_empty_args_list_are_valid() {
    let dir = tempfile::tempdir().unwrap();
    write_command(
        dir.path(),
        "clean.md",
        "---\ndescription: t\n\nargs: []\n---\nBody.\n",
    );
    let (commands, warnings) = load(dir.path(), &[]);
    assert_eq!(warnings, Vec::<String>::new());
    let clean = &commands["clean"];
    assert_eq!(clean.description, "t");
    assert!(clean.args.is_empty(), "`[]` declares no arguments");
    assert_eq!(clean.body, "Body.");
}

#[test]
fn an_unreadable_commands_path_warns_instead_of_panicking() {
    let dir = tempfile::tempdir().unwrap();
    // `.iris/commands` exists but is a file: `read_dir` fails with a
    // non-NotFound error, which must surface as a warning.
    fs::create_dir_all(dir.path().join(".iris")).unwrap();
    fs::write(dir.path().join(".iris").join("commands"), "i am a file").unwrap();
    let (commands, warnings) = load(dir.path(), &[]);
    assert!(commands.is_empty());
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("cannot read"), "{}", warnings[0]);
}

#[cfg(unix)]
#[test]
fn a_file_with_a_non_utf8_name_warns_and_is_skipped() {
    use std::os::unix::ffi::OsStrExt;
    let dir = tempfile::tempdir().unwrap();
    let cmd_dir = dir.path().join(".iris").join("commands");
    fs::create_dir_all(&cmd_dir).unwrap();
    fs::write(
        cmd_dir.join(std::ffi::OsStr::from_bytes(b"\xff.md")),
        "Body.\n",
    )
    .unwrap();
    let (commands, warnings) = load(dir.path(), &[]);
    assert!(commands.is_empty());
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(
        warnings[0].contains("not a usable command name"),
        "{}",
        warnings[0]
    );
}

#[test]
fn a_command_file_that_cannot_be_read_warns_and_the_rest_load() {
    let dir = tempfile::tempdir().unwrap();
    write_command(dir.path(), "good.md", "Fine.\n");
    // A *directory* named `trap.md`: the extension and stem look
    // right, but reading it as a file must fail into a warning.
    fs::create_dir_all(dir.path().join(".iris").join("commands").join("trap.md")).unwrap();
    let (commands, warnings) = load(dir.path(), &[]);
    assert!(commands.contains_key("good"), "the rest still load");
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("trap.md"), "{}", warnings[0]);
}
