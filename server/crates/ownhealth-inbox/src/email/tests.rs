//! Lane A unit tests: header facts, the allowlist decision and its authentication check.

use super::*;

const TRUSTED: &str = "mx.correo-salud.example";

fn rule(pattern: &str, profile: Uuid, trust_unauthenticated: bool) -> SenderRule {
    SenderRule {
        id: Uuid::new_v4(),
        source_id: Uuid::nil(),
        pattern: pattern.into(),
        profile_id: profile,
        doc_type_hint: None,
        trust_unauthenticated,
    }
}

fn facts(from: &str, auth: &[&str]) -> MessageFacts {
    MessageFacts {
        from: Some(from.into()),
        authentication_results: auth.iter().map(|s| s.to_string()).collect(),
        ..MessageFacts::default()
    }
}

fn reason(r: &Routing) -> &'static str {
    match r {
        Routing::Allowed { .. } => "allowed",
        Routing::Quarantine { reason } => reason,
    }
}

const RAW: &[u8] = b"Authentication-Results: mx.correo-salud.example;\r\n dkim=pass header.d=labnorte.example;\r\n spf=pass smtp.mailfrom=resultados@labnorte.example\r\n\
Authentication-Results: other.example; dkim=fail header.d=labnorte.example\r\n\
Message-ID: <ln-0001@labnorte.example>\r\n\
From: LabNorte Resultados <Resultados@LabNorte.example>\r\n\
To: salud.rivera@correo-salud.example\r\n\
Subject: =?UTF-8?Q?Resultados_anal=C3=ADticos_-_Alex_Rivera?=\r\n\
MIME-Version: 1.0\r\n\
Content-Type: multipart/mixed; boundary=\"b1\"\r\n\r\n\
--b1\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nAdjuntamos su informe.\r\n\
--b1\r\nContent-Type: application/pdf; name=\"informe.pdf\"\r\nContent-Disposition: attachment; filename=\"informe.pdf\"\r\n\
Content-Transfer-Encoding: base64\r\n\r\nJVBERi0xLjQKJSVFT0YK\r\n--b1--\r\n";

#[test]
fn facts_come_from_the_headers() {
    let f = message_facts(RAW).expect("parses");
    assert_eq!(f.from.as_deref(), Some("resultados@labnorte.example"));
    assert!(!f.multiple_from);
    assert_eq!(f.message_id.as_deref(), Some("ln-0001@labnorte.example"));
    assert_eq!(f.subject.as_deref(), Some("Resultados analíticos - Alex Rivera"));
    assert_eq!(f.authentication_results.len(), 2);
    assert!(f.authentication_results[0].starts_with("mx.correo-salud.example; dkim=pass"));
    assert_eq!(
        f.attachments,
        vec![(Some("informe.pdf".into()), "application/pdf".into(), 15)]
    );
}

#[test]
fn two_from_addresses_are_never_routed() {
    let raw = b"From: resultados@labnorte.example, otro@ofertas.example\r\nSubject: x\r\n\r\nhola\r\n";
    let f = message_facts(raw).expect("parses");
    assert!(f.multiple_from);
    assert_eq!(f.from, None);
    let p = Uuid::new_v4();
    let r = route_message(&f, &[rule("labnorte.example", p, true)], Some(TRUSTED));
    assert_eq!(reason(&r), "sender_not_allowed");
}

#[test]
fn an_allowlisted_sender_with_a_trusted_pass_is_routed_to_its_profile() {
    let alex = Uuid::new_v4();
    let carmen = Uuid::new_v4();
    let rules = [
        rule("labnorte.example", alex, false),
        rule("resultados@labnorte.example", carmen, false),
    ];
    let pass = format!("{TRUSTED}; dkim=pass header.d=labnorte.example");
    // the address rule beats the domain rule
    let r = route_message(&facts("resultados@labnorte.example", &[&pass]), &rules, Some(TRUSTED));
    assert!(matches!(r, Routing::Allowed { rule } if rule.profile_id == carmen));
    let r = route_message(&facts("citas@labnorte.example", &[&pass]), &rules, Some(TRUSTED));
    assert!(matches!(r, Routing::Allowed { rule } if rule.profile_id == alex));
    // a subdomain sender does not match a domain rule
    let r = route_message(&facts("x@mail.labnorte.example", &[&pass]), &rules, Some(TRUSTED));
    assert_eq!(reason(&r), "sender_not_allowed");
}

#[test]
fn spf_and_dmarc_passes_count_and_alignment_is_relaxed() {
    let p = Uuid::new_v4();
    let rules = [rule("labnorte.example", p, false)];
    let from = "resultados@labnorte.example";
    for header in [
        format!("{TRUSTED}; spf=pass smtp.mailfrom=bounces@labnorte.example"),
        format!("{TRUSTED}; dmarc=pass (p=reject) header.from=labnorte.example"),
        format!("{TRUSTED} 1; dkim=pass (2048-bit key) header.d=labnorte.example header.i=@labnorte.example"),
        // the parent domain signs for a subdomain sender
        format!("{TRUSTED}; dkim=pass header.d=labnorte.example"),
        "MX.Correo-Salud.example; spf=pass smtp.mailfrom=\"bounces@labnorte.example\"".to_string(),
    ] {
        let r = route_message(&facts(from, &[&header]), &rules, Some(TRUSTED));
        assert_eq!(reason(&r), "allowed", "{header}");
    }
    // a pass for another domain, or for a bare top-level domain, proves nothing
    for header in [
        format!("{TRUSTED}; dkim=pass header.d=ofertas.example"),
        format!("{TRUSTED}; dkim=pass header.d=example"),
        format!("{TRUSTED}; spf=pass smtp.mailfrom=x@labnorte.example.ofertas.example"),
        format!("{TRUSTED}; dkim=none; spf=neutral smtp.mailfrom=labnorte.example"),
    ] {
        let r = route_message(&facts(from, &[&header]), &rules, Some(TRUSTED));
        assert_eq!(reason(&r), "auth_failed", "{header}");
    }
}

#[test]
fn a_header_from_another_authserv_id_is_ignored() {
    let p = Uuid::new_v4();
    let rules = [rule("labnorte.example", p, false)];
    let forged = "mx.correo-salud.example.ofertas.example; dkim=pass header.d=labnorte.example";
    let comment = "(mx.correo-salud.example) ofertas.example; dkim=pass header.d=labnorte.example";
    for h in [forged, comment] {
        let r = route_message(&facts("resultados@labnorte.example", &[h]), &rules, Some(TRUSTED));
        assert_eq!(reason(&r), "auth_failed", "{h}");
    }
    // without a trusted authserv-id configured nothing authenticates
    let ok = format!("{TRUSTED}; dkim=pass header.d=labnorte.example");
    let r = route_message(&facts("resultados@labnorte.example", &[&ok]), &rules, None);
    assert_eq!(reason(&r), "auth_failed");
}

#[test]
fn a_forged_trusted_header_below_the_real_one_is_ignored() {
    let p = Uuid::new_v4();
    let rules = [rule("labnorte.example", p, false)];
    let real = format!("{TRUSTED}; dkim=fail header.d=labnorte.example; spf=fail smtp.mailfrom=labnorte.example");
    let forged = format!("{TRUSTED}; dkim=pass header.d=labnorte.example");
    let from = "resultados@labnorte.example";
    let r = route_message(&facts(from, &[&real, &forged]), &rules, Some(TRUSTED));
    assert_eq!(reason(&r), "auth_failed");
    // a real header that says nothing about this domain does not let a lower copy speak for it
    let silent = format!("{TRUSTED}; dkim=none");
    let r = route_message(&facts(from, &[&silent, &forged]), &rules, Some(TRUSTED));
    assert_eq!(reason(&r), "auth_failed");
    // headers of other servers above the trusted one are skipped, not counted as the top
    let other = "relay.ofertas.example; dkim=fail header.d=labnorte.example";
    let r = route_message(&facts(from, &[other, &forged]), &rules, Some(TRUSTED));
    assert_eq!(reason(&r), "allowed");
}

#[test]
fn trust_unauthenticated_needs_no_header_but_a_trusted_failure_still_holds() {
    let p = Uuid::new_v4();
    let rules = [rule("labnorte.example", p, true)];
    let from = "resultados@labnorte.example";
    assert_eq!(reason(&route_message(&facts(from, &[]), &rules, None)), "allowed");
    assert_eq!(
        reason(&route_message(&facts(from, &[]), &rules, Some(TRUSTED))),
        "allowed"
    );
    let fail = format!("{TRUSTED}; dkim=fail header.d=labnorte.example; spf=fail smtp.mailfrom=labnorte.example");
    assert_eq!(
        reason(&route_message(&facts(from, &[&fail]), &rules, Some(TRUSTED))),
        "auth_failed"
    );
    // and a sender nobody listed is held whatever its authentication says
    let pass = format!("{TRUSTED}; dkim=pass header.d=ofertas.example");
    let r = route_message(&facts("promo@ofertas.example", &[&pass]), &rules, Some(TRUSTED));
    assert_eq!(reason(&r), "sender_not_allowed");
    assert_eq!(
        reason(&route_message(&MessageFacts::default(), &rules, Some(TRUSTED))),
        "sender_not_allowed"
    );
}

#[test]
fn comments_are_stripped_without_touching_quotes() {
    assert_eq!(strip_comments("a (b (c)) d \"(e)\" f"), "a  d \"(e)\" f");
}

#[test]
fn patterns_are_addresses_or_domains() {
    assert_eq!(
        normalize_pattern(" Resultados@LabNorte.example ").ok().as_deref(),
        Some("resultados@labnorte.example")
    );
    assert_eq!(
        normalize_pattern("@labnorte.example").ok().as_deref(),
        Some("labnorte.example")
    );
    for bad in [
        "",
        "labnorte",
        "a@b@labnorte.example",
        "a b@labnorte.example",
        "@",
        "x@.example",
        "x@labnorte.",
    ] {
        assert!(normalize_pattern(bad).is_err(), "{bad}");
    }
}

#[test]
fn mailbox_config_needs_the_basics() {
    let ok = MailboxConfig {
        host: "imap.correo-salud.example".into(),
        port: 993,
        tls: "implicit".into(),
        username: "salud.rivera".into(),
        folder: inbox(),
        done_folder: done_folder(),
        trusted_authserv_id: None,
        ca_file: None,
    };
    assert!(ok.check().is_ok());
    assert!(
        MailboxConfig {
            tls: "ssl".into(),
            ..ok.clone()
        }
        .check()
        .is_err()
    );
    assert!(
        MailboxConfig {
            done_folder: "INBOX".into(),
            ..ok.clone()
        }
        .check()
        .is_err()
    );
    assert!(MailboxConfig { host: " ".into(), ..ok }.check().is_err());
}
