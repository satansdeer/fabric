use fabric_domain::{ErrorKind, HarnessRequest};
use serde_json::json;

#[test]
fn profiles_deny_unknown_fields_and_bound_nonsecret_references() {
    let base = json!({"harness_id":"fake-a","harness_version":"fabric-fake-a 1"});
    let valid: HarnessRequest = serde_json::from_value(json!({"profile":base})).unwrap();
    valid.validate().unwrap();
    for profile in [
        json!({"harness_id":"fake-a","harness_version":"fabric-fake-a 1","unexpected":"secret-sentinel"}),
        json!({"harness_id":"fake-a","harness_version":"fabric-fake-a 1","native_config_refs":[{"kind":"native-home","path":"/tmp","unexpected":"secret-sentinel"}]}),
        json!({"harness_id":"fake-a","harness_version":"fabric-fake-a 1","connection_refs":[{"kind":"env","name":"OPENAI_API_KEY","value":"secret-sentinel"}]}),
        json!({"harness_id":"fake-a","harness_version":"fabric-fake-a 1","model_policy":{"kind":"native-default","model":"ignored"}}),
        json!({"harness_id":"fake-a","harness_version":"fabric-fake-a 1","native_config_refs":[{"kind":"native-home","path":"relative"}]}),
        json!({"harness_id":"fake-a","harness_version":"fabric-fake-a 1","skills":vec!["entry";33]}),
    ] {
        assert!(serde_json::from_value::<HarnessRequest>(json!({"profile":profile})).is_err());
    }
    let mut invalid = valid;
    invalid.profile.as_mut().unwrap().harness_version = "secret-sentinel\n".into();
    let error = invalid.validate().unwrap_err();
    assert_eq!(error.kind, ErrorKind::InvalidProfile);
    assert!(!error.message.contains("secret-sentinel"));
}
