use std::collections::BTreeMap;

use fabric_domain::ErrorKind;
use fabric_project::recipe::{PREPARATION_FORMAT_VERSION, Recipe};

const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";
const RECIPE: &str = r#"
version = 1
environment = ["TOOLCHAIN", "TOKEN"]
[[steps]]
argv = ["sh", "-c", "printf ready > .prepared-marker"]
[limits]
max_duration_seconds = 1200
"#;

fn environment() -> BTreeMap<String, String> {
    BTreeMap::from([
        ("TOOLCHAIN".into(), "stable".into()),
        ("TOKEN".into(), "private-token-never-in-metadata".into()),
        ("UNDECLARED".into(), "ignored".into()),
    ])
}

#[test]
fn parses_argv_without_executing_and_hashes_semantics_not_toml_layout() {
    let recipe = Recipe::parse(RECIPE).unwrap();
    assert_eq!(
        recipe.steps()[0].argv(),
        ["sh", "-c", "printf ready > .prepared-marker"]
    );
    assert_eq!(recipe.max_duration_seconds(), 1200);
    assert_eq!(recipe.environment(), ["TOKEN", "TOOLCHAIN"]);
    let equivalent = Recipe::parse(
        r#"# reordered declarations and formatting
        environment = ["TOKEN", "TOOLCHAIN"]
        version = 1
        limits = { max_duration_seconds = 1200 }
        steps = [{ argv = ['sh', '-c', 'printf ready > .prepared-marker'] }]
        "#,
    )
    .unwrap();
    let identity = recipe.fingerprint(COMMIT, &environment()).unwrap();
    assert_eq!(
        identity,
        equivalent.fingerprint(COMMIT, &environment()).unwrap()
    );
    assert_eq!(identity.format_version, PREPARATION_FORMAT_VERSION);
    assert_eq!(identity.source_commit_sha, COMMIT);
    // Golden SHA-256 values computed independently with Python's hashlib/json.
    assert_eq!(
        identity.recipe_digest,
        "b6edb504be93a18496fcd7dfd455e2a7577fc233ae1018f3fe60920a831af723"
    );
    assert_eq!(
        identity.environment_digest,
        "e2b83c44407813a3c66d5788e8be9fcc8fffe0324932ac5268de5eb894c2c5bb"
    );
    assert_eq!(
        identity.preparation_key,
        "6b530ae696382f410696cf90e718cf704bf3b58bdbbdeef40c6b92a1c680fa75"
    );
    for digest in [
        &identity.recipe_digest,
        &identity.environment_digest,
        &identity.preparation_key,
    ] {
        assert_eq!(digest.len(), 64);
        assert!(
            digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        );
    }
    let metadata = serde_json::to_string(&identity).unwrap();
    assert!(!metadata.contains("private-token-never-in-metadata"));
    assert!(!format!("{identity:?}").contains("private-token-never-in-metadata"));
}

#[test]
fn identity_tracks_commit_argv_limits_and_only_declared_environment() {
    let recipe = Recipe::parse(RECIPE).unwrap();
    let mut env = environment();
    let original = recipe.fingerprint(COMMIT, &env).unwrap();
    env.insert("UNDECLARED".into(), "different".into());
    assert_eq!(original, recipe.fingerprint(COMMIT, &env).unwrap());
    env.insert("TOKEN".into(), "changed-secret".into());
    let changed = recipe.fingerprint(COMMIT, &env).unwrap();
    assert_ne!(original.preparation_key, changed.preparation_key);
    assert_eq!(original.recipe_digest, changed.recipe_digest);
    assert_ne!(original.environment_digest, changed.environment_digest);
    env.remove("TOKEN");
    let missing = recipe.fingerprint(COMMIT, &env).unwrap();
    env.insert("TOKEN".into(), String::new());
    assert_ne!(
        missing.preparation_key,
        recipe.fingerprint(COMMIT, &env).unwrap().preparation_key
    );
    assert_ne!(
        original.preparation_key,
        recipe
            .fingerprint(&"a".repeat(40), &environment())
            .unwrap()
            .preparation_key
    );
    assert_eq!(
        original,
        recipe
            .fingerprint(&COMMIT.to_uppercase(), &environment())
            .unwrap()
    );
    for changed_recipe in [
        RECIPE.replace("printf ready", "printf changed"),
        RECIPE.replace("1200", "1201"),
        RECIPE.replace("\"TOKEN\"", "\"OTHER\""),
    ] {
        let changed = Recipe::parse(&changed_recipe)
            .unwrap()
            .fingerprint(COMMIT, &environment())
            .unwrap();
        assert_ne!(original.recipe_digest, changed.recipe_digest);
        assert_ne!(original.preparation_key, changed.preparation_key);
    }
}

#[test]
fn minimal_recipe_defaults_are_canonical_and_argv_boundaries_are_preserved() {
    let minimal =
        Recipe::parse("version = 1\nsteps = [{ argv = ['echo', '', 'a b', ';', '$HOME'] }]")
            .unwrap();
    let explicit = Recipe::parse("version = 1\nenvironment = []\nsteps = [{ argv = ['echo', '', 'a b', ';', '$HOME'] }]\n[limits]\nmax_duration_seconds = 1200").unwrap();
    assert_eq!(
        minimal.fingerprint(COMMIT, &BTreeMap::new()).unwrap(),
        explicit.fingerprint(COMMIT, &BTreeMap::new()).unwrap()
    );
    let split =
        Recipe::parse("version = 1\nsteps = [{ argv = ['echo', '', 'a', 'b', ';', '$HOME'] }]")
            .unwrap();
    assert_ne!(
        minimal
            .fingerprint(COMMIT, &BTreeMap::new())
            .unwrap()
            .preparation_key,
        split
            .fingerprint(COMMIT, &BTreeMap::new())
            .unwrap()
            .preparation_key
    );
    let forward =
        Recipe::parse("version = 1\nsteps = [{ argv = ['one'] }, { argv = ['two'] }]").unwrap();
    let reverse =
        Recipe::parse("version = 1\nsteps = [{ argv = ['two'] }, { argv = ['one'] }]").unwrap();
    assert_ne!(
        forward
            .fingerprint(COMMIT, &BTreeMap::new())
            .unwrap()
            .preparation_key,
        reverse
            .fingerprint(COMMIT, &BTreeMap::new())
            .unwrap()
            .preparation_key
    );
}

#[test]
fn rejects_unknown_invalid_and_unexecutable_recipe_shapes() {
    for invalid in [
        "",
        "version = 2\nsteps = [{ argv = ['true'] }]",
        "steps = [{ argv = ['true'] }]",
        "version = '1'\nsteps = [{ argv = ['true'] }]",
        "version = 1\nsteps = []",
        "version = 1\nsteps = [{ argv = [] }]",
        "version = 1\nsteps = [{ argv = [''] }]",
        "version = 1\nsteps = [{ argv = ['  '] }]",
        "version = 1\nsteps = [{ argv = 'true' }]",
        "version = 1\nsteps = [{ argv = [1] }]",
        "version = 1\nsteps = [{ command = 'true' }]",
        "version = 1\nsteps = [{ argv = ['true'], cwd = '/' }]",
        "version = 1\nsteps = [{ argv = ['true'] }]\nunknown = true",
        "version = 1\nsteps = [{ argv = ['true'] }]\nlimits = { max_duration_seconds = 0 }",
        "version = 1\nsteps = [{ argv = ['true'] }]\nlimits = { max_duration_seconds = -1 }",
        "version = 1\nsteps = [{ argv = ['true'] }]\nlimits = { max_duration_seconds = 1.5 }",
        "version = 1\nsteps = [{ argv = ['true'] }]\nlimits = { max_duration_seconds = '1' }",
        "version = 1\nsteps = [{ argv = ['true'] }]\nlimits = { max_duration_seconds = 18446744073709551616 }",
        "version = 1\nsteps = [{ argv = ['true'] }]\nlimits = { max_duration_seconds = 1, other = 2 }",
        "version = 1\nsteps = [{ argv = ['true'] }]\nenvironment = { TOKEN = 'secret' }",
        "version = 1\nsteps = [{ argv = ['true'] }]\nenvironment = ['TOKEN', 'TOKEN']",
        "version = 1\nsteps = [{ argv = ['true'] }]\nenvironment = ['']",
        "version = 1\nsteps = [{ argv = ['true'] }]\nenvironment = ['1BAD']",
        "version = 1\nsteps = [{ argv = ['true'] }]\nenvironment = ['BAD=NAME']",
        "version = 1\nsteps = [{ argv = ['true', \"\\u0000\"] }]",
        "version = 1\nversion = 1\nsteps = [{ argv = ['true'] }]",
    ] {
        let error = Recipe::parse(invalid).unwrap_err();
        assert_eq!(error.kind, ErrorKind::InvalidRecipe, "{invalid}");
    }
    let error =
        Recipe::parse("version = 1\nsteps = [{ argv = 'do-not-echo-this-secret' }]").unwrap_err();
    assert!(!error.to_string().contains("do-not-echo-this-secret"));
    let recipe = Recipe::parse(RECIPE).unwrap();
    for commit in ["", "HEAD", "aabb", &"g".repeat(40), &"a".repeat(41)] {
        assert!(recipe.fingerprint(commit, &environment()).is_err());
    }
    assert!(recipe.fingerprint(&"a".repeat(64), &environment()).is_ok());
}
