//! Stage 5.2: golden redaction corpus. Each fixture in `tests/corpus/`
//! is a realistic shape of what tools actually surface — env dumps,
//! curl headers, patch hunks, error messages, private-key pastes, and
//! one clean-prose negative control. The `insta` snapshot pins the
//! exact bytes redaction produces, so any heuristic change becomes a
//! reviewable diff instead of a silent behavior shift.

use std::fs;
use std::path::PathBuf;

#[test]
fn golden_redaction_corpus() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/corpus");
    let mut fixtures: Vec<PathBuf> = fs::read_dir(&dir)
        .expect("corpus dir exists")
        .map(|entry| entry.expect("dir entry").path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "txt"))
        .collect();
    fixtures.sort();
    assert!(!fixtures.is_empty(), "corpus dir holds no fixtures");

    for path in fixtures {
        let name = path
            .file_stem()
            .expect("fixture stem")
            .to_string_lossy()
            .into_owned();
        let input = fs::read_to_string(&path).expect("readable fixture");
        let output = iris_core::redact::redact_text(&input).into_owned();
        insta::assert_snapshot!(name, output);
    }
}
