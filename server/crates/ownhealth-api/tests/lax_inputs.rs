//! REST inputs are read as pydantic's lax mode read them in 0.1.1: integral strings, integral
//! floats and bools are ints, a midnight datetime is a date, and a refused record lists every
//! failing field as `{type, loc, msg}`. The expected entries are pydantic 2's output.

#[path = "auth_support.rs"]
mod support;

use axum::http::StatusCode;
use serde_json::{Value, json};
use support::Env;

type R = anyhow::Result<()>;

#[tokio::test]
async fn token_days_take_what_pydantic_took() -> R {
    let env = Env::new(true).await?;
    let mut c = env.as_user(env.user("alex")).await?;
    let body = |days: Value| json!({"label": "Claude Code", "profiles": ["alex"], "days": days});
    for days in [json!("30"), json!(true), json!(30.0), json!(" 7 ")] {
        let r = c.post("/api/tokens", &body(days.clone())).await?;
        assert_eq!(r.status, StatusCode::CREATED, "{days}: {:?}", r.json);
    }
    let r = c.post("/api/tokens", &body(json!(30.5))).await?;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        r.json["detail"],
        json!([{"type": "int_from_float", "loc": ["body", "days"],
                "msg": "Input should be a valid integer, got a number with a fractional part", "input": 30.5}])
    );
    let r = c.post("/api/tokens", &body(json!("0"))).await?;
    assert_eq!(
        r.json["detail"],
        json!([{"type": "greater_than_equal", "loc": ["body", "days"],
                "msg": "Input should be greater than or equal to 1", "input": "0", "ctx": {"ge": 1}}])
    );
    Ok(())
}

#[tokio::test]
async fn audit_paging_reads_lax_ints_and_reports_the_raw_input() -> R {
    let env = Env::new(true).await?;
    let mut c = env.as_user(env.user("alex")).await?;
    assert_eq!(c.get("/api/audit?limit=1.0").await?.status, StatusCode::OK);
    assert_eq!(c.get("/api/audit?limit=+5&before=1_000").await?.status, StatusCode::OK);
    let r = c.get("/api/audit?limit=0&before=0").await?;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        r.json["detail"],
        json!([
            {"type": "greater_than_equal", "loc": ["query", "limit"],
             "msg": "Input should be greater than or equal to 1", "input": "0", "ctx": {"ge": 1}},
            {"type": "greater_than_equal", "loc": ["query", "before"],
             "msg": "Input should be greater than or equal to 1", "input": "0", "ctx": {"ge": 1}},
        ])
    );
    let r = c.get("/api/audit?limit=1.5").await?;
    assert_eq!(r.json["detail"][0]["type"], "int_parsing");
    Ok(())
}

#[tokio::test]
async fn a_midnight_datetime_is_a_date() -> R {
    let env = Env::new(true).await?;
    let mut c = env.as_user(env.user("alex")).await?;
    for from in [
        "2020-01-01T00:00:00",
        "2020-01-01T00:00:00Z",
        "2020-01-01%2000:00",
        "2020-01-01T00:00:00.000%2B00:00",
    ] {
        let r = c.get(&format!("/api/profiles/alex/timeline?from={from}")).await?;
        assert_eq!(r.status, StatusCode::OK, "{from}: {:?}", r.json);
    }
    let cases = [
        (
            "2020-01-01T10:00:00",
            "date_from_datetime_inexact",
            "Datetimes provided to dates should have zero time - e.g. be exact dates",
        ),
        (
            "2020-13-01",
            "date_from_datetime_parsing",
            "Input should be a valid date or datetime, month value is outside expected range of 1-12",
        ),
        (
            "2020-02-30",
            "date_from_datetime_parsing",
            "Input should be a valid date or datetime, day value is outside expected range",
        ),
        (
            "2020/01/01",
            "date_from_datetime_parsing",
            "Input should be a valid date or datetime, invalid date separator, expected `-`",
        ),
        (
            "yesterday",
            "date_from_datetime_parsing",
            "Input should be a valid date or datetime, input is too short",
        ),
    ];
    for (from, kind, msg) in cases {
        let r = c.get(&format!("/api/profiles/alex/timeline?from={from}")).await?;
        assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY, "{from}");
        assert_eq!(
            (&r.json["detail"][0]["type"], &r.json["detail"][0]["msg"]),
            (&json!(kind), &json!(msg)),
            "{from}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn a_refused_record_lists_every_field_as_pydantic_did() -> R {
    let env = Env::new(true).await?;
    let mut c = env.as_user(env.user("alex")).await?;
    let cases = [
        (
            "condition",
            json!({}),
            json!([{"type": "missing", "loc": ["text"], "msg": "Field required"}]),
        ),
        (
            "condition",
            json!({"text": 5, "onset": "2020-13-01", "verification_status": "maybe",
                   "sensitivity": ["x", "mental_health"], "effective_at": "yesterday"}),
            json!([
                {"type": "datetime_from_date_parsing", "loc": ["effective_at"],
                 "msg": "Input should be a valid datetime or date, input is too short"},
                {"type": "literal_error", "loc": ["sensitivity", 0],
                 "msg": "Input should be 'mental_health', 'reproductive' or 'sexual_health'"},
                {"type": "string_type", "loc": ["text"], "msg": "Input should be a valid string"},
                {"type": "literal_error", "loc": ["verification_status"],
                 "msg": "Input should be 'provisional', 'confirmed', 'refuted', 'unconfirmed', 'differential' or 'entered-in-error'"},
                {"type": "date_from_datetime_parsing", "loc": ["onset"],
                 "msg": "Input should be a valid date or datetime, month value is outside expected range of 1-12"},
            ]),
        ),
        (
            "medication",
            json!({"name": null, "kind": "drug", "started": "2020-01-01T10:00:00", "med_status": 3}),
            json!([
                {"type": "literal_error", "loc": ["kind"], "msg": "Input should be 'medication' or 'supplement'"},
                {"type": "string_type", "loc": ["name"], "msg": "Input should be a valid string"},
                {"type": "date_from_datetime_inexact", "loc": ["started"],
                 "msg": "Datetimes provided to dates should have zero time - e.g. be exact dates"},
                {"type": "literal_error", "loc": ["med_status"], "msg": "Input should be 'active', 'stopped' or 'on_hold'"},
            ]),
        ),
        (
            "immunization",
            json!({"vaccine": "x", "dose_number": "2.5", "occurred": [1]}),
            json!([
                {"type": "date_type", "loc": ["occurred"], "msg": "Input should be a valid date"},
                {"type": "int_parsing", "loc": ["dose_number"],
                 "msg": "Input should be a valid integer, unable to parse string as an integer"},
            ]),
        ),
        (
            "allergy",
            json!({"substance": "x", "criticality": "mid", "sensitivity": "genetic"}),
            json!([
                {"type": "list_type", "loc": ["sensitivity"], "msg": "Input should be a valid list"},
                {"type": "literal_error", "loc": ["criticality"], "msg": "Input should be 'low', 'high' or 'unable-to-assess'"},
            ]),
        ),
    ];
    for (kind, body, errors) in cases {
        let r = c.post(&format!("/api/profiles/alex/records/{kind}"), &body).await?;
        assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY, "{kind} {body}");
        assert_eq!(r.json["detail"]["errors"], errors, "{kind} {body}");
    }
    let lax = [
        (
            "immunization",
            json!({"vaccine": "Tdap", "dose_number": "2", "occurred": "2024-05-02T00:00:00Z"}),
        ),
        (
            "procedure",
            json!({"text": "Appendectomy", "performed": "2019-03-04 00:00"}),
        ),
    ];
    for (kind, body) in lax {
        let r = c.post(&format!("/api/profiles/alex/records/{kind}"), &body).await?;
        assert_eq!(r.status, StatusCode::CREATED, "{kind}: {:?}", r.json);
    }
    Ok(())
}
