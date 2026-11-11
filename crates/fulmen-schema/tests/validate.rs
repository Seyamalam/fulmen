//! Integration tests for the compiled validator: per-kind strict/lax
//! behavior, constraints, records, unions, errors — plus adversarial
//! inputs (ReDoS-shaped patterns, huge numbers, deep nesting).

use std::borrow::Cow;

use fulmen_schema::{
    Bound, ClassRef, EnumDesc, EnumKind, EnumMember, ErrorCode, ExtraPolicy, FieldDesc, IntDesc,
    Kind, Len, Limits, ListDesc, LiteralValue, MAX_ERRORS, Missing, Mode, Pattern, Range,
    RecordDesc, RecordKind, Source, StrDesc, Transforms, TypeDesc, TypeMeta, ValidationErrors,
    Value, compile, compile_with_limits,
};

fn s(text: &str) -> Value<'static> {
    Value::string(text.to_owned())
}

fn obj(pairs: Vec<(&str, Value<'static>)>) -> Value<'static> {
    Value::Object(
        pairs
            .into_iter()
            .map(|(k, v)| (Cow::Owned(k.to_owned()) as Cow<'static, str>, v))
            .collect(),
    )
}

fn arr(items: Vec<Value<'static>>) -> Value<'static> {
    Value::Array(items)
}

#[expect(
    clippy::unwrap_used,
    reason = "test helper: a compile/validate failure is a test failure"
)]
fn ok<'v>(d: &TypeDesc, v: &'v Value<'v>) -> Cow<'v, Value<'v>> {
    compile(d)
        .unwrap()
        .validate(v, Source::Body, Mode::Strict)
        .unwrap()
}

#[expect(
    clippy::unwrap_used,
    reason = "test helper: a compile/validate failure is a test failure"
)]
fn ok_lax<'v>(d: &TypeDesc, v: &'v Value<'v>) -> Cow<'v, Value<'v>> {
    compile(d)
        .unwrap()
        .validate(v, Source::Query, Mode::Lax)
        .unwrap()
}

/// The output value (deref'd) equals `expected`.
macro_rules! assert_out {
    ($d:expr, $v:expr, $expected:expr) => {
        assert_eq!(*ok($d, $v), $expected)
    };
}

macro_rules! assert_lax {
    ($d:expr, $v:expr, $expected:expr) => {
        assert_eq!(*ok_lax($d, $v), $expected)
    };
}

#[expect(
    clippy::unwrap_used,
    reason = "test helper: a compile/validate failure is a test failure"
)]
fn err(d: &TypeDesc, v: &Value<'_>, mode: Mode) -> ValidationErrors {
    let source = match mode {
        Mode::Strict => Source::Body,
        Mode::Lax => Source::Query,
    };
    compile(d).unwrap().validate(v, source, mode).unwrap_err()
}

fn code(d: &TypeDesc, v: &Value<'_>, mode: Mode) -> ErrorCode {
    err(d, v, mode).entries()[0].code
}

#[test]
fn scalars_strict() {
    assert_out!(&TypeDesc::bool(), &Value::Bool(true), Value::Bool(true));
    assert_eq!(
        code(&TypeDesc::bool(), &s("true"), Mode::Strict),
        ErrorCode::Type
    );
    // int — strict rejects strings, floats, bools.
    assert_out!(&TypeDesc::int(), &Value::Int(5), Value::Int(5));
    let big = Value::BigInt("123456789012345678901234567890".into());
    assert_out!(&TypeDesc::int(), &big, big.clone());
    assert_eq!(
        code(&TypeDesc::int(), &s("5"), Mode::Strict),
        ErrorCode::Type
    );
    assert_eq!(
        code(&TypeDesc::int(), &Value::Float(1.0), Mode::Strict),
        ErrorCode::Type
    );
    assert_eq!(
        code(&TypeDesc::int(), &Value::Bool(true), Mode::Strict),
        ErrorCode::Type
    );
    // float — strict accepts int → f64 coercion.
    assert_out!(&TypeDesc::float(), &Value::Int(2), Value::Float(2.0));
    assert_out!(&TypeDesc::str(), &s("hi"), s("hi"));
    assert_eq!(
        code(&TypeDesc::str(), &Value::Int(1), Mode::Lax),
        ErrorCode::Type
    );
    assert_out!(&TypeDesc::none(), &Value::Null, Value::Null);
    assert_eq!(code(&TypeDesc::none(), &s(""), Mode::Lax), ErrorCode::Type);
    // bytes — lax UTF-8 coercion.
    assert!(matches!(
        ok_lax(&TypeDesc::bytes(), &s("ab")),
        Cow::Owned(Value::Bytes(b)) if b.as_ref() == b"ab"
    ));
}

#[test]
fn scalars_lax() {
    assert!(matches!(
        ok_lax(&TypeDesc::bool(), &s("TRUE")),
        Cow::Owned(Value::Bool(true))
    ));
    assert!(matches!(
        ok_lax(&TypeDesc::bool(), &s("0")),
        Cow::Owned(Value::Bool(false))
    ));
    assert_eq!(code(&TypeDesc::bool(), &s("2"), Mode::Lax), ErrorCode::Bool);
    assert!(matches!(
        ok_lax(&TypeDesc::int(), &s("042")),
        Cow::Owned(Value::Int(42))
    ));
    assert!(matches!(
        ok_lax(&TypeDesc::int(), &s(" -7 ")),
        Cow::Owned(Value::Int(-7))
    ));
    assert!(matches!(
        ok_lax(&TypeDesc::int(), &s("123456789012345678901234567890")),
        Cow::Owned(Value::BigInt(t)) if t == "123456789012345678901234567890"
    ));
    assert_eq!(code(&TypeDesc::int(), &s("1.5"), Mode::Lax), ErrorCode::Int);
    assert_eq!(code(&TypeDesc::int(), &s("1e3"), Mode::Lax), ErrorCode::Int);
    assert_lax!(&TypeDesc::float(), &s("1.5"), Value::Float(1.5));
    assert!(matches!(
        ok_lax(&TypeDesc::float(), &s("inf")),
        Cow::Owned(Value::Float(f)) if f.is_infinite()
    ));
    // decimal — canonical output text.
    assert_out!(&TypeDesc::decimal(), &s("1.50"), s("1.5"));
    assert_out!(&TypeDesc::decimal(), &Value::Int(5), s("5"));
    assert_out!(&TypeDesc::decimal(), &Value::Float(0.25), s("0.25"));
    assert_eq!(
        code(&TypeDesc::decimal(), &s("1.2.3"), Mode::Strict),
        ErrorCode::Decimal
    );
    assert_eq!(
        code(&TypeDesc::decimal(), &Value::Float(f64::NAN), Mode::Strict),
        ErrorCode::Decimal
    );
}

#[test]
fn temporal_and_identifiers() {
    assert_out!(
        &TypeDesc::datetime(),
        &s("2024-01-31t10:20:30z"),
        s("2024-01-31T10:20:30Z")
    );
    assert_eq!(
        code(
            &TypeDesc::datetime(),
            &s("2024-02-30T00:00:00Z"),
            Mode::Strict
        ),
        ErrorCode::DateTime
    );
    assert_out!(&TypeDesc::date(), &s("2024-02-29"), s("2024-02-29"));
    assert_eq!(
        code(&TypeDesc::date(), &s("2024-02-30"), Mode::Strict),
        ErrorCode::Date
    );
    assert_out!(&TypeDesc::time(), &s("23:59"), s("23:59:00"));
    assert_out!(&TypeDesc::timedelta(), &s("PT1H30M"), s("P0DT1H30M0S"));
    assert_eq!(
        code(&TypeDesc::timedelta(), &s("1h30m"), Mode::Strict),
        ErrorCode::TimeDelta
    );
    // uuid canonicalization.
    assert_out!(
        &TypeDesc::uuid(),
        &s("{550E8400-E29B-41D4-A716-446655440000}"),
        s("550e8400-e29b-41d4-a716-446655440000")
    );
    assert_eq!(
        code(&TypeDesc::uuid(), &s("nope"), Mode::Lax),
        ErrorCode::Uuid
    );
    // email/url pragmatics.
    assert_out!(&TypeDesc::email(), &s("a@b.co"), s("a@b.co"));
    assert_eq!(
        code(&TypeDesc::email(), &s("a@b@c"), Mode::Strict),
        ErrorCode::Email
    );
    assert_out!(&TypeDesc::url(), &s("https://x.co/p"), s("https://x.co/p"));
    assert_eq!(
        code(&TypeDesc::url(), &s("x.co"), Mode::Strict),
        ErrorCode::Url
    );
    // IPs canonicalize.
    assert_out!(&TypeDesc::ipv6(), &s("0:0:0:0:0:0:0:1"), s("::1"));
    assert_eq!(
        code(&TypeDesc::ipv4(), &s("999.1.1.1"), Mode::Strict),
        ErrorCode::Ipv4
    );
}

#[test]
fn constraints() {
    // Len on str counts characters, not bytes.
    let mut sd = StrDesc::new();
    sd.len = Some(Len::new(Some(2), Some(3)));
    let d = TypeDesc::new(Kind::Str(sd));
    assert_out!(&d, &s("ab"), s("ab"));
    assert_out!(&d, &s("hél"), s("hél")); // 3 chars, 4 bytes
    assert_eq!(code(&d, &s("a"), Mode::Strict), ErrorCode::MinLen);
    assert_eq!(code(&d, &s("abcd"), Mode::Strict), ErrorCode::MaxLen);
    // Pattern.
    let mut sd = StrDesc::new();
    sd.pattern = Some(Pattern::new("^[a-z]+$").unwrap());
    let d = TypeDesc::new(Kind::Str(sd));
    assert_out!(&d, &s("abc"), s("abc"));
    assert_eq!(code(&d, &s("ABC"), Mode::Strict), ErrorCode::Pattern);
    // Range on int incl. `limit` in errors.
    let mut id = IntDesc::new();
    id.range = Some(Range {
        ge: Some(Bound::Int(1)),
        lt: Some(Bound::Int(10)),
        ..Range::default()
    });
    let d = TypeDesc::new(Kind::Int(id));
    assert_out!(&d, &Value::Int(5), Value::Int(5));
    let e = err(&d, &Value::Int(0), Mode::Strict);
    assert_eq!(e.entries()[0].code, ErrorCode::Ge);
    assert_eq!(e.entries()[0].limit, Some(LiteralValue::Int(1)));
    assert_eq!(e.entries()[0].pointer(), "/body");
    assert_eq!(code(&d, &Value::Int(10), Mode::Strict), ErrorCode::Lt);
    // Decimal bound vs int input — exact compare.
    let mut id = IntDesc::new();
    id.range = Some(Range::ge(Bound::Decimal("1.5".into())));
    let d = TypeDesc::new(Kind::Int(id));
    assert_eq!(code(&d, &Value::Int(1), Mode::Strict), ErrorCode::Ge);
    assert_out!(&d, &Value::Int(2), Value::Int(2));
    // MultipleOf.
    let mut id = IntDesc::new();
    id.multiple_of = Some(Bound::Int(3));
    let d = TypeDesc::new(Kind::Int(id));
    assert_out!(&d, &Value::Int(9), Value::Int(9));
    assert_eq!(
        code(&d, &Value::Int(7), Mode::Strict),
        ErrorCode::MultipleOf
    );
    // Decimal MultipleOf is exact.
    let mut dd = fulmen_schema::DecimalDesc::new();
    dd.multiple_of = Some(Bound::Decimal("0.5".into()));
    let d = TypeDesc::new(Kind::Decimal(dd));
    assert_out!(&d, &s("1.50"), s("1.5"));
    assert_eq!(code(&d, &s("1.51"), Mode::Strict), ErrorCode::MultipleOf);
    // Transforms run before checks.
    let d = TypeDesc::with_meta(
        Kind::Str(StrDesc {
            len: Some(Len::new(Some(3), Some(3))),
            pattern: None,
        }),
        TypeMeta {
            transforms: Transforms {
                strip: true,
                lower: false,
                upper: false,
            },
            ..TypeMeta::default()
        },
    );
    assert_out!(&d, &s("  abc  "), s("abc"));
    assert_eq!(code(&d, &s("  ab  "), Mode::Strict), ErrorCode::MinLen);
    // Transforms apply to lax scalar coercion too.
    let d = TypeDesc::with_meta(
        Kind::Bool,
        TypeMeta {
            transforms: Transforms {
                strip: true,
                lower: false,
                upper: false,
            },
            ..TypeMeta::default()
        },
    );
    assert!(matches!(
        ok_lax(&d, &s("  true ")),
        Cow::Owned(Value::Bool(true))
    ));
}

#[test]
fn literal_enum_optional() {
    let d = TypeDesc::literal(vec![
        LiteralValue::Int(1),
        LiteralValue::Str("x".into()),
        LiteralValue::Null,
    ]);
    assert_out!(&d, &Value::Int(1), Value::Int(1));
    assert_out!(&d, &s("x"), s("x"));
    assert_out!(&d, &Value::Null, Value::Null);
    assert_eq!(code(&d, &Value::Int(2), Mode::Strict), ErrorCode::Literal);
    // Lax reinterpretation: "1" → Int(1), "none" → Null.
    assert!(matches!(ok_lax(&d, &s("1")), Cow::Owned(Value::Int(1))));
    assert_lax!(&d, &s("none"), Value::Null);

    let color = TypeDesc::new(Kind::Enum(EnumDesc {
        class: ClassRef {
            name: "Color".into(),
            module: "m".into(),
        },
        kind: EnumKind::Str,
        members: vec![
            EnumMember {
                name: "RED".into(),
                value: LiteralValue::Str("red".into()),
            },
            EnumMember {
                name: "GREEN".into(),
                value: LiteralValue::Str("green".into()),
            },
        ]
        .into(),
    }));
    assert_out!(&color, &s("red"), s("red"));
    assert_eq!(code(&color, &s("blue"), Mode::Strict), ErrorCode::Enum);
    let int_enum = TypeDesc::new(Kind::Enum(EnumDesc {
        class: ClassRef {
            name: "Rank".into(),
            module: "m".into(),
        },
        kind: EnumKind::Int,
        members: vec![EnumMember {
            name: "A".into(),
            value: LiteralValue::Int(1),
        }]
        .into(),
    }));
    assert!(matches!(
        ok_lax(&int_enum, &s("1")),
        Cow::Owned(Value::Int(1))
    ));

    let d = TypeDesc::optional(TypeDesc::int());
    assert_out!(&d, &Value::Null, Value::Null);
    assert_out!(&d, &Value::Int(3), Value::Int(3));
    assert!(matches!(ok_lax(&d, &s("3")), Cow::Owned(Value::Int(3))));
    assert_eq!(code(&d, &s("x"), Mode::Lax), ErrorCode::Int);
}

#[test]
fn containers() {
    let d = TypeDesc::list(TypeDesc::int());
    let v = arr(vec![Value::Int(1), Value::Int(2)]);
    assert_out!(&d, &v, v.clone());
    let bad = arr(vec![Value::Int(1), s("x")]);
    let e = err(&d, &bad, Mode::Strict);
    assert_eq!(e.entries()[0].pointer(), "/body/1");
    assert_eq!(e.entries()[0].code, ErrorCode::Type);
    // Lax: single-value promotion + per-item coercion.
    assert_lax!(&d, &s("5"), arr(vec![Value::Int(5)]));
    assert_lax!(
        &d,
        &arr(vec![s("1"), s("2")]),
        arr(vec![Value::Int(1), Value::Int(2)])
    );
    // Len on items.
    let mut ld = ListDesc::new(TypeDesc::int());
    ld.len = Some(Len::new(Some(2), None));
    let d = TypeDesc::new(Kind::List(ld));
    assert_eq!(
        code(&d, &arr(vec![Value::Int(1)]), Mode::Strict),
        ErrorCode::MinLen
    );
    // Unique.
    let mut ld = ListDesc::new(TypeDesc::int());
    ld.unique = true;
    let d = TypeDesc::new(Kind::List(ld));
    assert_eq!(
        code(&d, &arr(vec![Value::Int(1), Value::Int(1)]), Mode::Strict),
        ErrorCode::Unique
    );
    // set dedupes (lax coercion makes "1"/"01" the same int).
    let d = TypeDesc::set(TypeDesc::int());
    assert_lax!(
        &d,
        &arr(vec![s("1"), s("01"), s("2")]),
        arr(vec![Value::Int(1), Value::Int(2)])
    );
    let d = TypeDesc::frozenset(TypeDesc::str());
    assert_out!(&d, &arr(vec![s("a"), s("a")]), arr(vec![s("a")]));
    // tuple var + fixed.
    let d = TypeDesc::tuple_var(TypeDesc::str());
    assert_out!(&d, &arr(vec![s("a")]), arr(vec![s("a")]));
    let d = TypeDesc::tuple_fixed(vec![TypeDesc::int(), TypeDesc::str()]);
    let v = arr(vec![Value::Int(1), s("x")]);
    assert_out!(&d, &v, v.clone());
    let e = err(&d, &arr(vec![Value::Int(1)]), Mode::Strict);
    assert_eq!(e.entries()[0].code, ErrorCode::TupleLen);
    assert_eq!(e.entries()[0].limit, Some(LiteralValue::UInt(2)));
    // dict: keys always lax — "02" → int key → canonical "2".
    let d = TypeDesc::dict(TypeDesc::int(), TypeDesc::int());
    assert_out!(
        &d,
        &obj(vec![("1", Value::Int(5)), ("02", Value::Int(6))]),
        obj(vec![("1", Value::Int(5)), ("2", Value::Int(6))])
    );
    let e = err(&d, &obj(vec![("x", Value::Int(1))]), Mode::Strict);
    assert_eq!(e.entries()[0].code, ErrorCode::Int);
    assert_eq!(e.entries()[0].pointer(), "/body/x");
}

#[test]
fn unions() {
    let d = TypeDesc::union(vec![TypeDesc::int(), TypeDesc::str()]);
    assert_out!(&d, &Value::Int(1), Value::Int(1));
    assert_out!(&d, &s("x"), s("x"));
    assert_eq!(code(&d, &Value::Bool(true), Mode::Strict), ErrorCode::Union);
    // "true" is not int text → falls through to the bool variant.
    let d = TypeDesc::union(vec![TypeDesc::int(), TypeDesc::bool()]);
    assert!(matches!(
        ok_lax(&d, &s("true")),
        Cow::Owned(Value::Bool(true))
    ));

    // Discriminated union.
    let pet = |name: &str, tag: &str, field: (&str, TypeDesc)| {
        TypeDesc::record(RecordDesc {
            class: ClassRef {
                name: name.into(),
                module: "m".into(),
            },
            kind: RecordKind::Model,
            fields: vec![
                FieldDesc::required(
                    "kind",
                    TypeDesc::literal(vec![LiteralValue::Str(tag.into())]),
                ),
                FieldDesc::required(field.0, field.1),
            ]
            .into(),
            extra: ExtraPolicy::default(),
            rename: None,
            type_params: Box::new([]),
        })
    };
    let d = TypeDesc::union_tagged(
        vec![
            pet("Cat", "cat", ("lives", TypeDesc::int())),
            pet("Dog", "dog", ("barks", TypeDesc::bool())),
        ],
        "kind",
    );
    let input = obj(vec![("kind", s("cat")), ("lives", Value::Int(9))]);
    let v = ok(&d, &input);
    assert!(matches!(v.as_ref(), Value::Object(_)));
    assert_eq!(
        code(&d, &obj(vec![("lives", Value::Int(9))]), Mode::Strict),
        ErrorCode::UnionTagMissing
    );
    assert_eq!(
        code(&d, &obj(vec![("kind", s("fish"))]), Mode::Strict),
        ErrorCode::UnionTagInvalid
    );
    // Field errors inside the chosen variant surface at field locs.
    let e = err(
        &d,
        &obj(vec![("kind", s("dog")), ("barks", s("maybe"))]),
        Mode::Strict,
    );
    assert!(e.entries().iter().any(|e| e.pointer() == "/body/barks"));
}

#[test]
fn records() {
    let user = TypeDesc::record(RecordDesc {
        class: ClassRef {
            name: "User".into(),
            module: "m".into(),
        },
        kind: RecordKind::Model,
        fields: vec![
            FieldDesc::required("name", TypeDesc::str()),
            FieldDesc {
                name: "age".into(),
                alias: None,
                missing: Missing::Literal(LiteralValue::Int(0)),
                ty: TypeDesc::int(),
                meta: None,
            },
            FieldDesc {
                name: "nick".into(),
                alias: None,
                missing: Missing::Omit,
                ty: TypeDesc::str(),
                meta: None,
            },
        ]
        .into(),
        extra: ExtraPolicy::Ignore,
        rename: None,
        type_params: Box::new([]),
    });
    let input = obj(vec![("name", s("ada"))]);
    let v = ok(&user, &input);
    match v {
        Cow::Owned(Value::Object(o)) => {
            assert_eq!(o.len(), 2); // name + injected default age; nick omitted
            assert_eq!(o[0].0.as_ref(), "name");
            assert_eq!(o[1].0.as_ref(), "age");
        }
        _ => panic!("expected owned object"),
    }
    // The identical fast path: input == output borrows the input.
    let input = obj(vec![("name", s("ada")), ("age", Value::Int(3))]);
    let v = ok(&user, &input);
    assert!(matches!(v, Cow::Borrowed(_)));
    // Extras ignored by default.
    let input = obj(vec![("name", s("ada")), ("junk", s("x"))]);
    let v = ok(&user, &input);
    assert!(matches!(v, Cow::Owned(Value::Object(o)) if o.len() == 2));
    let mut forbid = user.clone();
    if let Kind::Record(r) = &mut forbid.kind {
        r.extra = ExtraPolicy::Forbid;
    }
    let e = err(
        &forbid,
        &obj(vec![("name", s("ada")), ("junk", s("x"))]),
        Mode::Strict,
    );
    assert_eq!(e.entries()[0].code, ErrorCode::ExtraForbidden);
    assert_eq!(e.entries()[0].pointer(), "/body/junk");
    let mut keep = user.clone();
    if let Kind::Record(r) = &mut keep.kind {
        r.extra = ExtraPolicy::Keep;
    }
    let input = obj(vec![("name", s("ada")), ("junk", s("x"))]);
    let v = ok(&keep, &input);
    assert!(matches!(
        v,
        Cow::Owned(Value::Object(o)) if o.iter().any(|(k, _)| k.as_ref() == "junk")
    ));
    // Missing + a bad field: both errors collected.
    let e = err(&user, &obj(vec![("age", s("x"))]), Mode::Strict);
    let codes: Vec<_> = e.entries().iter().map(|e| e.code).collect();
    assert!(codes.contains(&ErrorCode::Missing)); // name
    assert!(codes.contains(&ErrorCode::Type)); // age str in strict
}

#[test]
fn record_wire_keys() {
    // Aliases: wire key is the alias, output key is the field name.
    let mut f = FieldDesc::required("user_name", TypeDesc::str());
    f.alias = Some("userName".into());
    let d = TypeDesc::record(RecordDesc {
        class: ClassRef {
            name: "A".into(),
            module: "m".into(),
        },
        kind: RecordKind::Model,
        fields: vec![f].into(),
        extra: ExtraPolicy::default(),
        rename: None,
        type_params: Box::new([]),
    });
    assert_out!(
        &d,
        &obj(vec![("userName", s("ada"))]),
        obj(vec![("user_name", s("ada"))])
    );
    let e = err(&d, &obj(vec![("user_name", s("ada"))]), Mode::Strict);
    assert_eq!(e.entries()[0].code, ErrorCode::Missing);
    assert_eq!(e.entries()[0].pointer(), "/body/userName");
    // rename="camel" applies to unaliased fields.
    let d = TypeDesc::record(RecordDesc {
        class: ClassRef {
            name: "B".into(),
            module: "m".into(),
        },
        kind: RecordKind::Model,
        fields: vec![FieldDesc::required("user_name", TypeDesc::str())].into(),
        extra: ExtraPolicy::default(),
        rename: Some(fulmen_schema::RenameRule::Camel),
        type_params: Box::new([]),
    });
    assert_out!(
        &d,
        &obj(vec![("userName", s("ada"))]),
        obj(vec![("user_name", s("ada"))])
    );
    // Nested records — error locs descend.
    let inner = TypeDesc::record(RecordDesc {
        class: ClassRef {
            name: "Addr".into(),
            module: "m".into(),
        },
        kind: RecordKind::Model,
        fields: vec![FieldDesc::required("city", TypeDesc::str())].into(),
        extra: ExtraPolicy::default(),
        rename: None,
        type_params: Box::new([]),
    });
    let outer = TypeDesc::record(RecordDesc {
        class: ClassRef {
            name: "U".into(),
            module: "m".into(),
        },
        kind: RecordKind::Model,
        fields: vec![FieldDesc::required("address", inner)].into(),
        extra: ExtraPolicy::default(),
        rename: None,
        type_params: Box::new([]),
    });
    let e = err(&outer, &obj(vec![("address", obj(vec![]))]), Mode::Strict);
    assert_eq!(e.entries()[0].pointer(), "/body/address/city");
    assert_eq!(e.entries()[0].code, ErrorCode::Missing);
}

#[test]
fn generics_and_newtype_any_raw() {
    // Page[int] — substitution reaches into the record's fields.
    let page = TypeDesc::record(RecordDesc {
        class: ClassRef {
            name: "Page".into(),
            module: "m".into(),
        },
        kind: RecordKind::Model,
        fields: vec![
            FieldDesc::required(
                "items",
                TypeDesc::list(TypeDesc::new(Kind::TypeVar("T".into()))),
            ),
            FieldDesc::required("total", TypeDesc::int()),
        ]
        .into(),
        extra: ExtraPolicy::default(),
        rename: None,
        type_params: vec!["T".into()].into(),
    });
    let applied = TypeDesc::new(Kind::Generic(fulmen_schema::GenericDesc {
        origin: Box::new(page),
        args: vec![TypeDesc::int()].into(),
    }));
    let v = obj(vec![
        ("items", arr(vec![Value::Int(1)])),
        ("total", Value::Int(1)),
    ]);
    assert_out!(&applied, &v, v.clone());
    let e = err(
        &applied,
        &obj(vec![("items", arr(vec![s("x")])), ("total", Value::Int(1))]),
        Mode::Strict,
    );
    assert_eq!(e.entries()[0].pointer(), "/body/items/0");
    // NewType validates as inner.
    let d = TypeDesc::newtype("UserId", TypeDesc::int());
    assert!(matches!(ok_lax(&d, &s("5")), Cow::Owned(Value::Int(5))));
    // Any/Raw pass through.
    let v = arr(vec![s("x")]);
    assert_out!(&TypeDesc::any(), &v, v.clone());
    let v = Value::Bytes(Cow::Borrowed(&b"abc"[..]));
    assert_out!(&TypeDesc::raw(), &v, v.clone());
}

#[test]
fn error_collection_and_shape() {
    // Every problem is collected; locs match §11.7.
    let user = TypeDesc::record(RecordDesc {
        class: ClassRef {
            name: "U".into(),
            module: "m".into(),
        },
        kind: RecordKind::Model,
        fields: vec![
            FieldDesc::required("email", TypeDesc::email()),
            FieldDesc::required("tags", TypeDesc::list(TypeDesc::int())),
        ]
        .into(),
        extra: ExtraPolicy::Forbid,
        rename: None,
        type_params: Box::new([]),
    });
    let e = err(
        &user,
        &obj(vec![
            ("email", s("nope")),
            ("tags", arr(vec![Value::Int(1), s("x"), s("y")])),
            ("extra1", s("z")),
            ("extra2", s("z")),
        ]),
        Mode::Strict,
    );
    let locs: Vec<String> = e
        .entries()
        .iter()
        .map(fulmen_schema::ErrorEntry::pointer)
        .collect();
    assert_eq!(
        locs,
        [
            "/body/email",
            "/body/tags/1",
            "/body/tags/2",
            "/body/extra1",
            "/body/extra2"
        ]
    );
    // §11.7 JSON shape.
    let json = e.to_json();
    assert!(json.contains("\"loc\":\"/body/email\",\"code\":\"email\""));
    assert!(json.contains("\"code\":\"extra_forbidden\""));
}

// ---- adversarial -------------------------------------------------------------

#[test]
fn adversarial_regex_is_linear() {
    // The classic catastrophic pattern `(a+)+$` — `regex` compiles it to a
    // finite automaton: linear time regardless of input.
    let mut sd = StrDesc::new();
    sd.pattern = Some(Pattern::new("^(a+)+$").unwrap());
    let d = TypeDesc::new(Kind::Str(sd));
    let start = std::time::Instant::now();
    let input = s(&format!("{}!", "a".repeat(50_000)));
    let e = err(&d, &input, Mode::Strict);
    assert_eq!(e.entries()[0].code, ErrorCode::Pattern);
    assert!(start.elapsed() < std::time::Duration::from_secs(2));
}

#[test]
fn adversarial_huge_numbers() {
    // A 200-digit integer — BigInt, no overflow, exact range checks.
    let big = "1".to_owned() + &"0".repeat(199);
    let mut id = IntDesc::new();
    id.range = Some(Range::ge(Bound::Int(i128::MAX)));
    let d = TypeDesc::new(Kind::Int(id));
    let v = Value::BigInt(big.into());
    assert_out!(&d, &v, v.clone());
    // Huge decimal exponent — canonical output stays a dozen bytes.
    let input = s("1e999999999");
    let out = ok(&TypeDesc::decimal(), &input);
    assert!(matches!(out, Cow::Owned(Value::Str(t)) if t == "1E+999999999"));
    // Massive digit string exercises the digit-walk multiple_of.
    let mut id = IntDesc::new();
    id.multiple_of = Some(Bound::Int(3));
    let d = TypeDesc::new(Kind::Int(id));
    let v = Value::BigInt("3".repeat(1000).into());
    assert_out!(&d, &v, v.clone());
}

#[test]
fn adversarial_deep_nesting() {
    // `Any` passes content through untouched — the validator never
    // recurses into it, so arbitrarily deep payloads are safe (decoder
    // limits cover unvalidated input).
    let d = TypeDesc::list(TypeDesc::any());
    let mut v = Value::Int(1);
    for _ in 0..200 {
        v = arr(vec![v]);
    }
    assert!(
        compile(&d)
            .unwrap()
            .validate(&v, Source::Body, Mode::Strict)
            .is_ok()
    );
    // Validator recursion is bounded by descriptor depth: a descriptor
    // deeper than max_depth errors instead of overflowing the stack.
    let d = TypeDesc::list(TypeDesc::list(TypeDesc::list(TypeDesc::any())));
    let v = arr(vec![arr(vec![arr(vec![Value::Int(1)])])]);
    let limits = Limits {
        max_depth: 2,
        ..Limits::default()
    };
    let res = compile_with_limits(&d, limits)
        .unwrap()
        .validate(&v, Source::Body, Mode::Strict);
    assert!(
        res.unwrap_err()
            .entries()
            .iter()
            .any(|e| e.code == ErrorCode::MaxDepth)
    );
    // max_items.
    let limits = Limits {
        max_items: 3,
        ..Limits::default()
    };
    let big = arr(vec![Value::Int(0); 10]);
    let res = compile_with_limits(&d, limits)
        .unwrap()
        .validate(&big, Source::Body, Mode::Strict);
    assert_eq!(res.unwrap_err().entries()[0].code, ErrorCode::MaxItems);
    // The error list caps at MAX_ERRORS.
    let d = TypeDesc::list(TypeDesc::int());
    let big = arr((0..200).map(|_| s("x")).collect());
    let e = err(&d, &big, Mode::Strict);
    assert_eq!(e.len(), MAX_ERRORS);
    assert!(e.truncated());
}

#[test]
fn source_is_recorded() {
    let e = err(&TypeDesc::int(), &s("x"), Mode::Lax);
    assert_eq!(e.entries()[0].pointer(), "/query");
    let e = err(&TypeDesc::int(), &s("x"), Mode::Strict);
    assert_eq!(e.entries()[0].pointer(), "/body");
}
