use std::path::Path;

#[test]
fn corpus_parses_and_every_runnable_case_has_expectations() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../conformance/cases");
    let corpus = spanbolt_conformance::case::load(&root).unwrap_or_else(|e| panic!("{e}"));
    assert!(corpus.cases.len() >= 300, "{} cases", corpus.cases.len());
    for case in corpus.cases.iter().filter(|c| c.skip.is_none()) {
        assert!(
            case.steps.iter().any(|s| s.expect.is_some()),
            "{} has no recorded expectation",
            case.name
        );
    }
}
