use fabric_domain::{Capability, ErrorKind, HarnessRequest};

#[test]
fn request_defaults_and_strict_untrusted_configuration() {
    let request: HarnessRequest = serde_json::from_str("{}").unwrap();
    assert_eq!(request.placement, "local");
    assert_eq!(request.access, "edit");
    assert!(request.model.is_none());
    assert!(request.required_capabilities.is_empty());
    assert!(request.validate().is_ok());
    for invalid in [
        r#"{"unknown":true}"#,
        r#"{"profile":"future"}"#,
        r#"{"placement":"typo"}"#,
        r#"{"access":"typo"}"#,
        r#"{"model":" "}"#,
        r#"{"resume":""}"#,
        r#"{"model":"line\nbreak"}"#,
        r#"{"max_cost_usd":0}"#,
        r#"{"max_cost_usd":-1}"#,
        r#"{"required_capabilities":["made.up"]}"#,
    ] {
        assert!(
            serde_json::from_str::<HarnessRequest>(invalid).is_err(),
            "{invalid}"
        );
    }
    let request: HarnessRequest = serde_json::from_str(
        r#"{"model":"requested","resume":"session-ref","placement":"remote","access":"inspect","max_cost_usd":0.01}"#,
    )
    .unwrap();
    assert_eq!(request.model.as_deref(), Some("requested"));
    assert_eq!(request.resume.as_deref(), Some("session-ref"));
    assert_eq!(request.max_cost_usd, Some(0.01));
}

#[test]
fn direct_requests_cannot_bypass_validation() {
    for cost in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, 0.0, -1.0] {
        let request = HarnessRequest {
            max_cost_usd: Some(cost),
            ..HarnessRequest::default()
        };
        assert_eq!(
            request.validate().unwrap_err().kind,
            ErrorKind::InvalidArguments
        );
    }
    for request in [
        HarnessRequest {
            placement: "typo".into(),
            ..HarnessRequest::default()
        },
        HarnessRequest {
            access: "typo".into(),
            ..HarnessRequest::default()
        },
        HarnessRequest {
            resume: Some("\0".into()),
            ..HarnessRequest::default()
        },
    ] {
        assert_eq!(
            request.validate().unwrap_err().kind,
            ErrorKind::InvalidArguments
        );
    }
}

#[test]
fn all_capabilities_round_trip_the_exact_public_vocabulary() {
    assert_eq!(Capability::ALL.len(), 13);
    for capability in Capability::ALL {
        assert_eq!(
            capability.as_str().parse::<Capability>().unwrap(),
            capability
        );
        assert_eq!(
            serde_json::to_value(capability).unwrap(),
            serde_json::Value::String(capability.as_str().into())
        );
        assert_eq!(
            serde_json::from_value::<Capability>(serde_json::json!(capability.as_str())).unwrap(),
            capability
        );
    }
    assert!("session_resume".parse::<Capability>().is_err());
}
