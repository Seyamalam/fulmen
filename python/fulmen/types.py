"""Fulmen's own annotation types (DESIGN.md §11.1).

`Email` and `Url` are `str` subclasses used as type annotations — the
schema layer validates the wire value, then materializes the declared
class. `Raw` is the escape-hatch annotation for the undecoded body.
"""

from __future__ import annotations


class Email(str):
    """An email address, validated per §11.1's pragmatic rules.

    Validation happens in the Rust schema layer; this class adds no checks
    of its own so already-validated values wrap for free.
    """


class Url(str):
    """A URL (`scheme://host…`), validated per §11.1.

    Same note as `Email`: the schema layer owns validation; this is a
    `str` marker class.
    """


class Raw:
    """Annotation-only marker: the undecoded request body.

    `body: Raw` (or `Annotated[bytes, Raw]`) declares a body parameter that
    receives the raw payload with no JSON decode or schema validation.
    """

    __slots__ = ()
