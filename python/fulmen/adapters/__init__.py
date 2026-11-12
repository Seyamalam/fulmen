"""Built-in adapters (DESIGN.md §15).

Adapters integrate third-party schema/infra libraries without changing
Fulmen's core contract: the library's own types become spec trees, and the
same `Descriptor` machinery validates them natively.
"""
