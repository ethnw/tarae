use super::*;
use crate::sandbox::tests::env;

/// frictions29 30: macOS's tools say a refusal as `<program>: <name>: <refusal>` — `touch: Makefile: Operation not
/// permitted`, all a run's output says when the kernel's own report is lost: read as the protected name refused, from
/// where the run ran, not "the report names no cause". One the rules let it write, or a name with no program before
/// it, names nothing.
#[test]
fn a_tools_own_name_then_the_file_is_read_as_its_refusal() {
    let p = Policy::for_project(&env(Path::new("/Users/u/p")));
    let said = |p: &Policy, line: &str| -> Vec<String> {
        p.said_in_output(&[(false, line.to_string())], &[]).into_iter().map(|d| d.target).collect()
    };
    assert_eq!(said(&p, "touch: Makefile: Operation not permitted"), ["/Users/u/p/Makefile"]);
    // (each system's words: Landlock's "Permission denied" is a refusal only on Linux)
    let why = if cfg!(target_os = "linux") { "Permission denied" } else { "Operation not permitted" };
    assert_eq!(said(&p, &format!("mkdir: .github: {why}")), ["/Users/u/p/.github"]);
    let sub = Policy { cwd: Some(PathBuf::from("/Users/u/p/sub")), ..p.clone() };
    assert_eq!(said(&sub, "touch: Makefile: Operation not permitted"), ["/Users/u/p/sub/Makefile"]);
    assert_eq!(said(&sub, "touch: ../build.rs: Operation not permitted"), ["/Users/u/p/build.rs"]);
    for line in [
        "touch: src/a.rs: Operation not permitted",
        "Makefile: Operation not permitted",
        "touch: two words: Operation not permitted",
    ] {
        assert!(said(&p, line).is_empty(), "{line}");
    }
}
