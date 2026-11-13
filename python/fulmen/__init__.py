"""Fulmen: a hyper-fast, type-safe Python backend framework with a Rust core."""

import typing
from importlib import import_module

from fulmen._fulmen import StartupError, __version__, native_components

if typing.TYPE_CHECKING:
    from fulmen.adapters.native import NativeValidator
    from fulmen.adapters.pydantic import PydanticValidator
    from fulmen.app import (
        HOOK_PHASES,
        PRIORITY_SECURITY,
        PRIORITY_SYSTEM,
        PRIORITY_USER,
        Fulmen,
    )
    from fulmen.constraints import (
        AfterValidator,
        Alias,
        Default,
        Deprecated,
        Description,
        Discriminator,
        Example,
        Field,
        Len,
        Lower,
        MultipleOf,
        Pattern,
        Range,
        Strip,
        Unique,
        Upper,
    )
    from fulmen.errors import (
        BadGateway,
        BadRequest,
        Conflict,
        Forbidden,
        GatewayTimeout,
        Gone,
        HTTPError,
        InternalServerError,
        LengthRequired,
        MethodNotAllowed,
        NotAcceptable,
        NotFound,
        NotImplemented,
        PayloadTooLarge,
        PreconditionFailed,
        RequestTimeout,
        ServiceUnavailable,
        TooManyRequests,
        Unauthorized,
        UnprocessableContent,
        UnsupportedMediaType,
        URITooLong,
        ValidationError,
    )
    from fulmen.model import Model, ModelConfig
    from fulmen.params import (
        Body,
        Cookie,
        File,
        Form,
        Header,
        Path,
        Query,
        Request,
        UploadFile,
    )
    from fulmen.plugins import CheckRule, Plugin, PluginContext, PluginHandle
    from fulmen.request import Headers
    from fulmen.responses import (
        Accepted,
        Created,
        FileResponse,
        HTMLResponse,
        JSONResponse,
        NoContent,
        Redirect,
        Response,
        Stream,
        TextResponse,
    )
    from fulmen.settings import Secret, Settings
    from fulmen.types import Email, Raw, Url

#: Public name -> the submodule that defines it. Everything except the
#: native extension resolves on first access, so `import fulmen` pulls
#: no framework code — the `fulmen-testkit` pytest plugin registers
#: without dragging the package in ahead of coverage, and importing the
#: adapters' optional dependencies (pydantic) never happens by side
#: effect.
_MODULES: dict[str, str] = {
    "HOOK_PHASES": "fulmen.app",
    "PRIORITY_SECURITY": "fulmen.app",
    "PRIORITY_SYSTEM": "fulmen.app",
    "PRIORITY_USER": "fulmen.app",
    "Accepted": "fulmen.responses",
    "AfterValidator": "fulmen.constraints",
    "Alias": "fulmen.constraints",
    "BadGateway": "fulmen.errors",
    "BadRequest": "fulmen.errors",
    "Body": "fulmen.params",
    "CheckRule": "fulmen.plugins",
    "Conflict": "fulmen.errors",
    "Cookie": "fulmen.params",
    "Created": "fulmen.responses",
    "Default": "fulmen.constraints",
    "Deprecated": "fulmen.constraints",
    "Description": "fulmen.constraints",
    "Discriminator": "fulmen.constraints",
    "Email": "fulmen.types",
    "Example": "fulmen.constraints",
    "Field": "fulmen.constraints",
    "File": "fulmen.params",
    "FileResponse": "fulmen.responses",
    "Forbidden": "fulmen.errors",
    "Form": "fulmen.params",
    "Fulmen": "fulmen.app",
    "GatewayTimeout": "fulmen.errors",
    "Gone": "fulmen.errors",
    "HTMLResponse": "fulmen.responses",
    "HTTPError": "fulmen.errors",
    "Header": "fulmen.params",
    "Headers": "fulmen.request",
    "InternalServerError": "fulmen.errors",
    "JSONResponse": "fulmen.responses",
    "Len": "fulmen.constraints",
    "LengthRequired": "fulmen.errors",
    "Lower": "fulmen.constraints",
    "MethodNotAllowed": "fulmen.errors",
    "Model": "fulmen.model",
    "ModelConfig": "fulmen.model",
    "MultipleOf": "fulmen.constraints",
    "NativeValidator": "fulmen.adapters.native",
    "NoContent": "fulmen.responses",
    "NotAcceptable": "fulmen.errors",
    "NotFound": "fulmen.errors",
    "NotImplemented": "fulmen.errors",
    "Path": "fulmen.params",
    "Pattern": "fulmen.constraints",
    "PayloadTooLarge": "fulmen.errors",
    "Plugin": "fulmen.plugins",
    "PluginContext": "fulmen.plugins",
    "PluginHandle": "fulmen.plugins",
    "PreconditionFailed": "fulmen.errors",
    "PydanticValidator": "fulmen.adapters.pydantic",
    "Query": "fulmen.params",
    "Range": "fulmen.constraints",
    "Raw": "fulmen.types",
    "Redirect": "fulmen.responses",
    "Request": "fulmen.params",
    "RequestTimeout": "fulmen.errors",
    "Response": "fulmen.responses",
    "Secret": "fulmen.settings",
    "ServiceUnavailable": "fulmen.errors",
    "Settings": "fulmen.settings",
    "Stream": "fulmen.responses",
    "Strip": "fulmen.constraints",
    "TextResponse": "fulmen.responses",
    "TooManyRequests": "fulmen.errors",
    "URITooLong": "fulmen.errors",
    "Unauthorized": "fulmen.errors",
    "Unique": "fulmen.constraints",
    "UnprocessableContent": "fulmen.errors",
    "UnsupportedMediaType": "fulmen.errors",
    "UploadFile": "fulmen.params",
    "Upper": "fulmen.constraints",
    "Url": "fulmen.types",
    "ValidationError": "fulmen.errors",
}


def __getattr__(name: str) -> object:
    # `from fulmen import X` resolves `X` on demand; submodule names
    # (`fulmen.errors`, …) import lazily the same way.
    module = _MODULES.get(name)
    if module is not None:
        value = getattr(import_module(module), name)
        globals()[name] = value  # resolved once, then a plain attribute
        return value
    try:
        return import_module(f"{__name__}.{name}")
    except ModuleNotFoundError as exc:
        if exc.name != f"{__name__}.{name}":
            raise
        raise AttributeError(f"module {__name__!r} has no attribute {name!r}") from exc


def __dir__() -> list[str]:
    return sorted(__all__)


__all__ = [
    "HOOK_PHASES",
    "PRIORITY_SECURITY",
    "PRIORITY_SYSTEM",
    "PRIORITY_USER",
    "Accepted",
    "AfterValidator",
    "Alias",
    "BadGateway",
    "BadRequest",
    "Body",
    "CheckRule",
    "Conflict",
    "Cookie",
    "Created",
    "Default",
    "Deprecated",
    "Description",
    "Discriminator",
    "Email",
    "Example",
    "Field",
    "File",
    "FileResponse",
    "Forbidden",
    "Form",
    "Fulmen",
    "GatewayTimeout",
    "Gone",
    "HTMLResponse",
    "HTTPError",
    "Header",
    "Headers",
    "InternalServerError",
    "JSONResponse",
    "Len",
    "LengthRequired",
    "Lower",
    "MethodNotAllowed",
    "Model",
    "ModelConfig",
    "MultipleOf",
    "NativeValidator",
    "NoContent",
    "NotAcceptable",
    "NotFound",
    "NotImplemented",
    "Path",
    "Pattern",
    "PayloadTooLarge",
    "Plugin",
    "PluginContext",
    "PluginHandle",
    "PreconditionFailed",
    "PydanticValidator",
    "Query",
    "Range",
    "Raw",
    "Redirect",
    "Request",
    "RequestTimeout",
    "Response",
    "Secret",
    "ServiceUnavailable",
    "Settings",
    "StartupError",
    "Stream",
    "Strip",
    "TextResponse",
    "TooManyRequests",
    "URITooLong",
    "Unauthorized",
    "Unique",
    "UnprocessableContent",
    "UnsupportedMediaType",
    "UploadFile",
    "Upper",
    "Url",
    "ValidationError",
    "__version__",
    "native_components",
]
