//! `decode_and_validate` must never panic on arbitrary bytes — the full
//! request-body pipeline, decode + strict/lax validation, against a
//! descriptor exercising every validator arm (ROADMAP M2.19).

#![no_main]

use std::sync::OnceLock;

use fulmen_schema::{
    Bound, ClassRef, ExtraPolicy, FieldDesc, IntDesc, Kind, Len, LiteralValue, Mode,
    Pattern, Range, RecordDesc, RecordKind, Source, StrDesc, TypeDesc, Validator, compile,
    decode_and_validate, decode_json,
};
use libfuzzer_sys::fuzz_target;

fn f(name: &str, ty: TypeDesc) -> FieldDesc {
    FieldDesc::required(name, ty)
}

fn record(name: &str, fields: Vec<FieldDesc>, extra: ExtraPolicy) -> TypeDesc {
    TypeDesc::record(RecordDesc {
        class: ClassRef {
            name: name.into(),
            module: "fuzz".into(),
        },
        kind: RecordKind::Model,
        fields: fields.into(),
        extra,
        rename: None,
        type_params: [].into(),
    })
}

/// One descriptor touching every `validate` arm: constrained scalars,
/// canonicalizing parsers, containers, unions, literals, nested records.
fn desc() -> TypeDesc {
    record(
        "M",
        vec![
            f("b", TypeDesc::bool()),
            f("i", TypeDesc::int()),
            f(
                "i_ranged",
                TypeDesc::new(Kind::Int(IntDesc {
                    range: Some(Range {
                        ge: Some(Bound::Int(-100)),
                        gt: None,
                        le: Some(Bound::Int(100)),
                        lt: None,
                    }),
                    multiple_of: None,
                })),
            ),
            f("num", TypeDesc::float()),
            f("s", TypeDesc::str()),
            f(
                "s_len",
                TypeDesc::new(Kind::Str(StrDesc {
                    len: Some(Len::new(Some(1), Some(8))),
                    pattern: None,
                })),
            ),
            f(
                "s_pat",
                TypeDesc::new(Kind::Str(StrDesc {
                    len: None,
                    pattern: Some(Pattern::new("^[a-z]{1,4}$").expect("static pattern compiles")),
                })),
            ),
            f("dec", TypeDesc::decimal()),
            f("dt", TypeDesc::datetime()),
            f("d", TypeDesc::date()),
            f("t", TypeDesc::time()),
            f("td", TypeDesc::timedelta()),
            f("u", TypeDesc::uuid()),
            f("e", TypeDesc::email()),
            f("url", TypeDesc::url()),
            f("ip4", TypeDesc::ipv4()),
            f("ip6", TypeDesc::ipv6()),
            f("xs", TypeDesc::list(TypeDesc::int())),
            f("set_i", TypeDesc::set(TypeDesc::int())),
            f("dict_si", TypeDesc::dict(TypeDesc::str(), TypeDesc::int())),
            f(
                "tup",
                TypeDesc::tuple_fixed(vec![TypeDesc::int(), TypeDesc::str()]),
            ),
            f("tup_v", TypeDesc::tuple_var(TypeDesc::int())),
            f("opt", TypeDesc::optional(TypeDesc::int())),
            f(
                "u_",
                TypeDesc::union(vec![TypeDesc::int(), TypeDesc::str(), TypeDesc::bool()]),
            ),
            f(
                "lit",
                TypeDesc::literal(vec![
                    LiteralValue::Int(1),
                    LiteralValue::Str("x".into()),
                    LiteralValue::Null,
                ]),
            ),
            f("any", TypeDesc::any()),
            f("raw", TypeDesc::raw()),
            f(
                "nested",
                record(
                    "N",
                    vec![f("y", TypeDesc::int())],
                    ExtraPolicy::Forbid,
                ),
            ),
        ],
        ExtraPolicy::Ignore,
    )
}

fn validator() -> &'static Validator {
    static V: OnceLock<Validator> = OnceLock::new();
    V.get_or_init(|| compile(&desc()).expect("static desc compiles"))
}

fuzz_target!(|data: &[u8]| {
    let v = validator();
    // Strict: the request-body pipeline.
    let _ = decode_and_validate(data, v);
    // Lax: same input, coercion mode — covers the arms strict skips.
    if let Ok(input) = decode_json(data) {
        let _ = v.validate_out(&input, Source::Body, Mode::Lax);
    }
});
