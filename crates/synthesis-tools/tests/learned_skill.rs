//! Learned skills come from a callback (no database in this crate). Own test binary: the hooks are
//! process-wide, one per session id.

use std::sync::Arc;

use synthesis_config::Env;
use synthesis_tools::{LearnedSkill, SkillSource, discover_session_skills, set_learned_skills};

#[test]
fn learned_skills_are_listed_as_learned_and_can_be_cleared() {
    let d = tempfile::tempdir().unwrap();
    let env = Env::from_pairs([("SYNT_HOME", d.path().to_str().unwrap())]);
    let list = discover_session_skills(&env, d.path(), Some("s1"));
    assert!(list.iter().all(|s| s.source != SkillSource::Learned));

    set_learned_skills(
        "s1",
        Some(Arc::new(|| {
            vec![
                LearnedSkill {
                    name: "seed".into(),
                    description: "[learned, ok 7/8] run with the seed".into(),
                    body: "PARSER_SEED=1".into(),
                },
                LearnedSkill {
                    name: "bad name with spaces".into(),
                    description: "x".into(),
                    body: "x".into(),
                },
            ]
        })),
    );
    let list = discover_session_skills(&env, d.path(), Some("s1"));
    let s = list.iter().find(|s| s.name == "learned:seed").unwrap();
    assert_eq!((s.source, s.trusted), (SkillSource::Learned, true));
    assert!(s.description.starts_with("[learned, ok 7/8]"));
    assert!(
        list.iter().all(|s| !s.name.contains(' ')),
        "an invalid name is skipped"
    );

    // two sessions in one process: each sees its own source, and clearing one leaves the other
    set_learned_skills(
        "s2",
        Some(Arc::new(|| {
            vec![LearnedSkill {
                name: "other".into(),
                description: "d".into(),
                body: "b".into(),
            }]
        })),
    );
    let names = |sid| {
        discover_session_skills(&env, d.path(), Some(sid))
            .into_iter()
            .map(|s| s.name)
            .collect::<Vec<_>>()
    };
    assert!(names("s1").contains(&"learned:seed".into()));
    assert!(!names("s1").contains(&"learned:other".into()));
    assert!(names("s2").contains(&"learned:other".into()));
    set_learned_skills("s2", None);
    assert!(names("s1").contains(&"learned:seed".into()));

    set_learned_skills("s1", None);
    assert!(
        discover_session_skills(&env, d.path(), Some("s1"))
            .iter()
            .all(|s| s.source != SkillSource::Learned)
    );
}
