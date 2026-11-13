"""Built-in adapters (DESIGN.md §15).

Adapters integrate third-party schema/infra libraries without changing
Fulmen's core contract: the library's own types become spec trees, and the
same `Descriptor` machinery validates them natively.

* `interfaces` — the five adapter `Protocol`s and their shared types
  (`Rate`, `Decision`, `Job`, `JobId`, `AdapterError`…) mirroring the
  `fulmen-core` traits.
* `memory` — the `Memory*` in-process implementations of the four
  store protocols.
* `native` — `NativeValidator`, the default `Validator` (native spec
  compiler).
* `pydantic` — `PydanticValidator`, the pydantic-model adapter
  (optional dependency).

Everything stays lazy: `import fulmen.adapters` pulls nothing beyond
this docstring; each name resolves on first access, so `pydantic` is
only imported when the adapter is actually used.
"""

from __future__ import annotations

import typing

if typing.TYPE_CHECKING:
    # Redundant-alias re-exports so `from fulmen.adapters import X` types
    # correctly while staying lazy at runtime.
    from fulmen.adapters.interfaces import (
        AdapterError as AdapterError,
    )
    from fulmen.adapters.interfaces import (
        CacheStore as CacheStore,
    )
    from fulmen.adapters.interfaces import (
        CompiledValidator as CompiledValidator,
    )
    from fulmen.adapters.interfaces import (
        Decision as Decision,
    )
    from fulmen.adapters.interfaces import (
        Job as Job,
    )
    from fulmen.adapters.interfaces import (
        JobId as JobId,
    )
    from fulmen.adapters.interfaces import (
        JobQueue as JobQueue,
    )
    from fulmen.adapters.interfaces import (
        NotFoundError as NotFoundError,
    )
    from fulmen.adapters.interfaces import (
        Rate as Rate,
    )
    from fulmen.adapters.interfaces import (
        RateLimitStore as RateLimitStore,
    )
    from fulmen.adapters.interfaces import (
        SessionStore as SessionStore,
    )
    from fulmen.adapters.interfaces import (
        StoreFullError as StoreFullError,
    )
    from fulmen.adapters.interfaces import (
        StoreUnavailableError as StoreUnavailableError,
    )
    from fulmen.adapters.interfaces import (
        UnsupportedError as UnsupportedError,
    )
    from fulmen.adapters.interfaces import (
        Validator as Validator,
    )
    from fulmen.adapters.memory import (
        MemoryCacheStore as MemoryCacheStore,
    )
    from fulmen.adapters.memory import (
        MemoryJobQueue as MemoryJobQueue,
    )
    from fulmen.adapters.memory import (
        MemoryRateLimitStore as MemoryRateLimitStore,
    )
    from fulmen.adapters.memory import (
        MemorySessionStore as MemorySessionStore,
    )
    from fulmen.adapters.native import NativeValidator as NativeValidator
    from fulmen.adapters.pydantic import PydanticValidator as PydanticValidator

_INTERFACES = {
    "AdapterError",
    "CacheStore",
    "CompiledValidator",
    "Decision",
    "Job",
    "JobId",
    "JobQueue",
    "NotFoundError",
    "Rate",
    "RateLimitStore",
    "SessionStore",
    "StoreFullError",
    "StoreUnavailableError",
    "UnsupportedError",
    "Validator",
}
_MEMORY = {
    "MemoryCacheStore",
    "MemoryJobQueue",
    "MemoryRateLimitStore",
    "MemorySessionStore",
}


def __getattr__(name: str) -> object:
    if name in _INTERFACES:
        from fulmen.adapters import interfaces

        return getattr(interfaces, name)
    if name in _MEMORY:
        from fulmen.adapters import memory

        return getattr(memory, name)
    if name == "NativeValidator":
        from fulmen.adapters.native import NativeValidator

        return NativeValidator
    if name == "PydanticValidator":
        from fulmen.adapters.pydantic import PydanticValidator

        return PydanticValidator
    raise AttributeError(f"module {__name__!r} has no attribute {name!r}")


__all__ = [
    "AdapterError",
    "CacheStore",
    "CompiledValidator",
    "Decision",
    "Job",
    "JobId",
    "JobQueue",
    "MemoryCacheStore",
    "MemoryJobQueue",
    "MemoryRateLimitStore",
    "MemorySessionStore",
    "NativeValidator",
    "NotFoundError",
    "PydanticValidator",
    "Rate",
    "RateLimitStore",
    "SessionStore",
    "StoreFullError",
    "StoreUnavailableError",
    "UnsupportedError",
    "Validator",
]
