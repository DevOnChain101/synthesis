//! Learned skills come from a callback (no database in this crate). Own test binary: the hook is
//! process-wide.

use std::sync::Arc;

use synthesis_config::Env;
use synthesis_tools::{LearnedSkill, SkillSource, discover_skills, set_learned_skills};

#[test]
fn learned_skills_are_listed_as_learned_and_can_be_cleared() {
    let d = tempfile::tempdir().unwrap();
    let env = Env::from_pairs([("SYNT_HOME", d.path().to_str().unwrap())]);
    let list = discover_skills(&env, d.path());
    assert!(list.iter().all(|s| s.source != SkillSource::Learned));

    set_learned_skills(Some(Arc::new(|| {
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
    })));
    let list = discover_skills(&env, d.path());
    let s = list.iter().find(|s| s.name == "learned:seed").unwrap();
    assert_eq!((s.source, s.trusted), (SkillSource::Learned, true));
    assert!(s.description.starts_with("[learned, ok 7/8]"));
    assert!(
        list.iter().all(|s| !s.name.contains(' ')),
        "an invalid name is skipped"
    );

    set_learned_skills(None);
    assert!(
        discover_skills(&env, d.path())
            .iter()
            .all(|s| s.source != SkillSource::Learned)
    );
}
