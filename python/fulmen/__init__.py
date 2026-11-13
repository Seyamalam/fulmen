"""Fulmen: a hyper-fast, type-safe Python backend framework with a Rust core."""

import typing

from fulmen._di import Provider, Scope, provider
from fulmen._fulmen import StartupError, __version__, native_components
from fulmen.app import HOOK_PHASES, PRIORITY_SECURITY, PRIORITY_SYSTEM, PRIORITY_USER, Fulmen
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

if typing.TYPE_CHECKING:
    from fulmen.adapters.native import NativeValidator
    from fulmen.adapters.pydantic import PydanticValidator


def __getattr__(name: str) -> object:
    # Adapters stay lazy: `fulmen.*Validator` resolves on first
    # access so `import fulmen` never pulls adapter code (or pydantic).
    if name == "PydanticValidator":
        from fulmen.adapters.pydantic import PydanticValidator

        return PydanticValidator
    if name == "NativeValidator":
        from fulmen.adapters.native import NativeValidator

        return NativeValidator
    raise AttributeError(f"module {__name__!r} has no attribute {name!r}")


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
    "Provider",
    "PydanticValidator",
    "Query",
    "Range",
    "Raw",
    "Redirect",
    "Request",
    "RequestTimeout",
    "Response",
    "Scope",
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
    "provider",
]
