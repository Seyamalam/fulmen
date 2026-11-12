//! Integration tests for the request pipeline: byte-path JSON decoding
//! composed with validation, the lax text-source adapters, the §11.7
//! error wire shape, and the no-input-echo guarantee.

use std::borrow::Cow;

use fulmen_schema::{
    ClassRef, DecodeError, ErrorCode, ExtraPolicy, FieldDesc, JsonErrorCode, Limits, LocSeg, Mode,
    PayloadError, RecordDesc, RecordKind, Source, SourceError, TextErrorCode, TypeDesc, Value,
    compile, compile_with_limits, decode_and_validate, decode_cookie, decode_form, decode_json,
    decode_json_with_limits, decode_query, validate_text,
};

fn user() -> TypeDesc {
    TypeDesc::record(RecordDesc {
        class: ClassRef {
            name: "User".into(),
            module: "m".into(),
        },
        kind: RecordKind::Model,
        fields: vec![
            FieldDesc::required("name", TypeDesc::str()),
            FieldDesc::required("age", TypeDesc::int()),
        ]
        .into(),
        extra: ExtraPolicy::Ignore,
        rename: None,
        type_params: Box::new([]),
    })
}

#[test]
fn decode_then_validate() {
    let v = compile(&user()).unwrap();
    // Valid body → Ok; the value borrows the request bytes.
    let out = decode_and_validate(br#"{"name":"ada","age":36}"#, &v).unwrap();
    let Value::Object(o) = out.as_ref() else {
        panic!("expected object")
    };
    assert_eq!(o.len(), 2);
    // Record output is rewritten (canonical field order) → Cow::Owned.
    assert!(matches!(out, Cow::Owned(_)));
    // A schema whose output is identical to its input stays `Same` → the
    // owned decode is still returned as `Cow::Owned` (nothing external
    // to borrow).
    let v = compile(&TypeDesc::any()).unwrap();
    let out = decode_and_validate(b"[1,2]", &v).unwrap();
    assert_eq!(*out, Value::Array(vec![Value::Int(1), Value::Int(2)]));
}

#[test]
fn malformed_vs_validation() {
    let v = compile(&user()).unwrap();
    // Truncated body → 400-shaped JsonError.
    match decode_and_validate(br#"{"name":"ada","age":"#, &v) {
        Err(PayloadError::Malformed(e)) => {
            assert_eq!(e.code, JsonErrorCode::Eof);
            assert!(e.position > 0);
        }
        r => panic!("expected malformed, got {r:?}"),
    }
    // Type error → 422-shaped ValidationErrors at field locs.
    match decode_and_validate(br#"{"name":"ada","age":"old"}"#, &v) {
        Err(PayloadError::Validation(e)) => {
            assert_eq!(e.entries()[0].code, ErrorCode::Type);
            assert_eq!(e.entries()[0].pointer(), "/body/age");
        }
        r => panic!("expected validation, got {r:?}"),
    }
    // Well-formed but too deep → 422 Limit, even under Any/Raw where the
    // validator never descends.
    let v = compile(&TypeDesc::any()).unwrap();
    let deep = "[".repeat(65) + &"]".repeat(65);
    match decode_and_validate(deep.as_bytes(), &v) {
        Err(PayloadError::Limit(e)) => {
            assert_eq!(e.entries()[0].code, ErrorCode::MaxDepth);
            assert_eq!(e.entries()[0].pointer(), "/body");
        }
        r => panic!("expected limit, got {r:?}"),
    }
    assert_eq!(
        PayloadError::status(&PayloadError::Malformed(fulmen_schema::JsonError {
            code: JsonErrorCode::Eof,
            position: 0,
            message: "x".into(),
        })),
        400
    );
}

#[test]
fn real_byte_path() {
    // Real `&[u8]` input, including raw non-ASCII bytes and escapes.
    let body =
        b"{\"s\":\"\xC3\xA9\",\"esc\":\"a\\nb\",\"u\":\"\\u00e9\",\"big\":18446744073709551616}";
    let v = decode_json(body).unwrap();
    assert_eq!(v.get("s"), Some(&Value::str("é")));
    assert_eq!(v.get("esc"), Some(&Value::str("a\nb")));
    assert_eq!(v.get("u"), Some(&Value::str("é")));
    assert_eq!(
        v.get("big"),
        Some(&Value::BigInt("18446744073709551616".into()))
    );
    // Unescaped strings borrow the input; escaped ones are owned.
    let v = decode_json(b"\"plain\"").unwrap();
    assert!(matches!(v, Value::Str(Cow::Borrowed(_))));
    let v = decode_json(b"\"a\\nb\"").unwrap();
    assert!(matches!(v, Value::Str(Cow::Owned(_))));
    // Raw invalid UTF-8 → InvalidUtf8, at the bad byte's position.
    match decode_json(b"\"\xff\"") {
        Err(DecodeError::Malformed(e)) => {
            assert_eq!(e.code, JsonErrorCode::InvalidUtf8);
            assert_eq!(e.position, 1);
        }
        r => panic!("expected invalid UTF-8, got {r:?}"),
    }
}

#[test]
fn decode_limits() {
    let limits = Limits {
        max_depth: 4,
        max_items: 2,
        max_str_bytes: 3,
        ..Limits::default()
    };
    for input in [
        "[[[[1]]]]",              // depth 5 > 4
        "[1,2,3]",                // items 3 > 2
        r#"{"a":1,"b":2,"c":3}"#, // entries 3 > 2
        r#""abcd""#,              // string 4 > 3
    ] {
        match decode_json_with_limits(input.as_bytes(), &limits) {
            Err(DecodeError::Limit(e)) => {
                assert!(matches!(
                    e.entries()[0].code,
                    ErrorCode::MaxDepth | ErrorCode::MaxItems | ErrorCode::MaxStrLen
                ));
            }
            r => panic!("expected limit for {input:?}, got {r:?}"),
        }
    }
    assert!(decode_json_with_limits(b"[[[1]]]", &limits).is_ok());
}

#[test]
fn query_to_record() {
    // `?tags=1&tags=2&name=ada` → Object {tags: ["1","2"], name: "ada"}
    // → lax list[int]/str validation coerces each item.
    let d = TypeDesc::record(RecordDesc {
        class: ClassRef {
            name: "Q".into(),
            module: "m".into(),
        },
        kind: RecordKind::Model,
        fields: vec![
            FieldDesc::required("tags", TypeDesc::list(TypeDesc::int())),
            FieldDesc::required("name", TypeDesc::str()),
        ]
        .into(),
        extra: ExtraPolicy::Ignore,
        rename: None,
        type_params: Box::new([]),
    });
    let v = compile(&d).unwrap();
    let input = decode_query("tags=1&tags=2&name=ada").unwrap();
    let out = v.validate(&input, Source::Query, Mode::Lax).unwrap();
    let Value::Object(o) = out.as_ref() else {
        panic!("expected object")
    };
    assert_eq!(
        o.iter().find(|(k, _)| k.as_ref() == "tags").map(|(_, v)| v),
        Some(&Value::Array(vec![Value::Int(1), Value::Int(2)]))
    );
    // `?page` is present-but-empty — distinguishable from absent, and it
    // fails `int` as an empty string would.
    let d = TypeDesc::record(RecordDesc {
        class: ClassRef {
            name: "P".into(),
            module: "m".into(),
        },
        kind: RecordKind::Model,
        fields: vec![FieldDesc::required("page", TypeDesc::int())].into(),
        extra: ExtraPolicy::Ignore,
        rename: None,
        type_params: Box::new([]),
    });
    let v = compile(&d).unwrap();
    let input = decode_query("page").unwrap();
    assert!(input.get("page").is_some()); // presence preserved
    let errs = v.validate(&input, Source::Query, Mode::Lax).unwrap_err();
    assert_eq!(errs.entries()[0].code, ErrorCode::Int);
    assert_eq!(errs.entries()[0].pointer(), "/query/page");
    // A wholly absent key reports `missing`, not `int`.
    let input = decode_query("other=1").unwrap();
    let errs = v.validate(&input, Source::Query, Mode::Lax).unwrap_err();
    assert_eq!(errs.entries()[0].code, ErrorCode::Missing);
}

#[test]
fn form_and_cookie_and_scalar() {
    // Form bodies reuse the query grammar; locs render `/form`.
    let v = compile(&TypeDesc::int()).unwrap();
    let input = decode_form("n=42").unwrap();
    let _ = input;
    let err = validate_text("x", &v, Source::Form).unwrap_err();
    assert_eq!(err.entries()[0].code, ErrorCode::Int);
    // Scalar path param: "42" → Int(42) via validate_text.
    let out = validate_text("42", &v, Source::Path).unwrap();
    assert_eq!(*out, Value::Int(42));
    // Header values are opaque strings — validate them as str/enum/&c.
    let v = compile(&TypeDesc::str()).unwrap();
    let out = validate_text("Bearer tok", &v, Source::Header).unwrap();
    assert_eq!(*out, Value::str("Bearer tok"));
    // Optional header: empty string is still a present value.
    let v = compile(&TypeDesc::optional(TypeDesc::int())).unwrap();
    let out = validate_text("7", &v, Source::Header).unwrap();
    assert_eq!(*out, Value::Int(7));
    // Cookie header → object → record in lax mode.
    let d = TypeDesc::record(RecordDesc {
        class: ClassRef {
            name: "C".into(),
            module: "m".into(),
        },
        kind: RecordKind::Model,
        fields: vec![
            FieldDesc::required("session", TypeDesc::str()),
            FieldDesc::required("n", TypeDesc::int()),
        ]
        .into(),
        extra: ExtraPolicy::Ignore,
        rename: None,
        type_params: Box::new([]),
    });
    let v = compile(&d).unwrap();
    let input = decode_cookie("session=abc; n=2").unwrap();
    let out = v.validate(&input, Source::Cookie, Mode::Lax).unwrap();
    let Value::Object(o) = out.as_ref() else {
        panic!("expected object")
    };
    assert_eq!(
        o.iter().find(|(k, _)| k.as_ref() == "n").map(|(_, v)| v),
        Some(&Value::Int(2))
    );
    // Malformed percent-encoding → structured 400-shaped error.
    match decode_query("a=%zz") {
        Err(SourceError::Malformed(e)) => {
            assert_eq!(e.code, TextErrorCode::PercentEscape);
            assert_eq!(e.position, 2);
        }
        r => panic!("expected malformed, got {r:?}"),
    }
}

#[test]
fn wire_error_shape() {
    // A record with two failures renders the §11.7 array.
    let v = compile(&user()).unwrap();
    let errs = v
        .validate(
            &decode_json(br#"{"age":"old","extra":1}"#).unwrap(),
            Source::Body,
            Mode::Strict,
        )
        .unwrap_err();
    // Entries follow field-check order (checked fields before missing).
    let json = errs.to_json();
    assert_eq!(
        json,
        "[{\"loc\":\"/body/age\",\"code\":\"type\",\"message\":\"expected int, got string\"},{\"loc\":\"/body/name\",\"code\":\"missing\",\"message\":\"required field missing\"}]"
    );
    // ProblemEntry views carry the same data, plus typed loc segments.
    let entries = errs.to_problem_entries();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].loc_pointer, "/body/age");
    assert_eq!(entries[0].code, "type");
    assert_eq!(entries[0].source, Source::Body);
    assert_eq!(entries[0].loc, &[LocSeg::Key("age".into())]);
    assert_eq!(entries[1].loc_pointer, "/body/name");
    assert!(entries[0].limit.is_none());
    // loc_pointer() is the same string as pointer().
    assert_eq!(errs.entries()[0].loc_pointer(), "/body/age");
    // A bound error's limit serializes through LiteralValue::to_json.
    let mut id = fulmen_schema::IntDesc::new();
    id.range = Some(fulmen_schema::Range::ge(fulmen_schema::Bound::Int(18)));
    let v = compile(&TypeDesc::new(fulmen_schema::Kind::Int(id))).unwrap();
    let errs = v
        .validate(&Value::Int(3), Source::Body, Mode::Strict)
        .unwrap_err();
    assert_eq!(
        errs.to_json(),
        "[{\"loc\":\"/body\",\"code\":\"ge\",\"message\":\"must be >= 18\",\"limit\":18}]"
    );
    assert_eq!(errs.to_problem_entries()[0].limit.unwrap().to_json(), "18");
}

#[test]
fn errors_never_echo_input() {
    // A secret-shaped input must not appear in any rendered error.
    let needle = "hunter2-must-stay-hidden";
    let mut sd = fulmen_schema::StrDesc::new();
    sd.pattern = Some(fulmen_schema::Pattern::new("^[a-z]+$").unwrap());
    let d = TypeDesc::record(RecordDesc {
        class: ClassRef {
            name: "Creds".into(),
            module: "m".into(),
        },
        kind: RecordKind::Model,
        fields: vec![
            FieldDesc::required("password", TypeDesc::new(fulmen_schema::Kind::Str(sd))),
            FieldDesc::required("pin", TypeDesc::int()),
        ]
        .into(),
        extra: ExtraPolicy::Forbid,
        rename: None,
        type_params: Box::new([]),
    });
    let v = compile(&d).unwrap();
    // Pattern failure on the password + int failure on pin + a surprise
    // key whose *name* echoes (names are locations, not values).
    let body = format!(r#"{{"password":"{needle}","pin":"{needle}","{needle}":1}}"#);
    let errs = decode_and_validate(body.as_bytes(), &v).unwrap_err();
    let PayloadError::Validation(errs) = errs else {
        panic!("expected validation errors")
    };
    for e in errs.entries() {
        assert!(!e.message.contains(needle), "echoed in message: {e:?}");
        // loc keys come from the input — that is a location, not a
        // value, so `/{needle}` in a pointer is expected; check
        // message and limit fields only.
    }
    // The message bodies and limit fields are what could leak a value.
    for e in errs.to_problem_entries() {
        assert!(!e.message.contains(needle));
        if let Some(l) = e.limit {
            assert!(!l.to_json().contains(needle));
        }
    }
    // Lax sources: a bad int must say "not a valid integer", not the text.
    let v = compile(&TypeDesc::int()).unwrap();
    let errs = validate_text(needle, &v, Source::Query).unwrap_err();
    assert_eq!(errs.entries()[0].code, ErrorCode::Int);
    assert!(!errs.entries()[0].message.contains(needle));
    assert!(!errs.to_json().contains(needle));
    // Malformed JSON errors describe positions, not content.
    let bad = format!(r#"{{"password":"{needle}"#);
    match decode_json(bad.as_bytes()) {
        Err(DecodeError::Malformed(e)) => {
            assert!(!e.message.contains(needle));
            assert!(!format!("{e}").contains(needle));
        }
        r => panic!("expected malformed, got {r:?}"),
    }
}

#[test]
fn any_and_raw_bounded_at_decode() {
    // Any/Raw accept everything the decoder produces — so the decode-time
    // caps are their only bounds.
    for kind in [TypeDesc::any(), TypeDesc::raw()] {
        let v = compile_with_limits(
            &kind,
            Limits {
                max_depth: 3,
                ..Limits::default()
            },
        )
        .unwrap();
        let deep = "[[[[1]]]]";
        match decode_and_validate(deep.as_bytes(), &v) {
            Err(PayloadError::Limit(e)) => {
                assert_eq!(e.entries()[0].code, ErrorCode::MaxDepth);
            }
            r => panic!("expected depth limit for {kind:?}, got {r:?}"),
        }
        let ok = decode_and_validate(b"[[1]]", &v).unwrap();
        assert!(matches!(ok.as_ref(), Value::Array(_)));
    }
}
