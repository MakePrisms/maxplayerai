use mobee_evals::{load_scenarios, run_and_grade, scenario_dir, snapshot_dir};

#[test]
fn scenarios_pass_deterministic_graders() {
    let scenarios = load_scenarios(&scenario_dir()).expect("load scenarios");
    assert!(!scenarios.is_empty(), "expected at least one scenario");
    let snapshot_root = snapshot_dir();
    let bless = std::env::var_os("MOBEE_EVALS_BLESS").is_some_and(|value| value == "1");
    let mut failures = Vec::new();

    for scenario in scenarios {
        if let Err(findings) = run_and_grade(&scenario, &snapshot_root, bless) {
            failures.push(format!("{}:", scenario.name));
            failures.extend(
                findings
                    .into_iter()
                    .map(|finding| format!("  {}: {}", finding.grader, finding.detail)),
            );
        }
    }

    assert!(failures.is_empty(), "\n{}", failures.join("\n"));
}
