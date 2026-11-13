//! The hookable pipeline phases (DESIGN.md §8) — the index values that
//! order [`crate::FulmenPluginDescriptorV1::hooks`].

/// The eight hookable phases, in pipeline order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Phase {
    /// Before any routing work: request-id, trusted-proxy resolution.
    OnRequest,
    /// Once the route is known: CORS preflight.
    BeforeRoute,
    /// Allow/deny checks: rate limit, authentication, CSRF, custom guards.
    Guard,
    /// Dependency resolution (the native DI seam).
    Resolve,
    /// Immediately before the handler.
    BeforeHandle,
    /// After the handler; a `Respond` decision replaces the response.
    AfterHandle,
    /// After the response is encoded; header mutation over any answer.
    OnResponse,
    /// After the response is produced; fire-and-forget (access log,
    /// metrics, background work). Response mutations land on a clone.
    AfterResponse,
}

impl Phase {
    /// Every phase, in pipeline order.
    pub const ALL: [Self; crate::abi::PHASE_COUNT] = [
        Self::OnRequest,
        Self::BeforeRoute,
        Self::Guard,
        Self::Resolve,
        Self::BeforeHandle,
        Self::AfterHandle,
        Self::OnResponse,
        Self::AfterResponse,
    ];

    /// The phase's index — positions in `descriptor.hooks` and `cx.phase`.
    pub fn index(self) -> usize {
        match self {
            Self::OnRequest => 0,
            Self::BeforeRoute => 1,
            Self::Guard => 2,
            Self::Resolve => 3,
            Self::BeforeHandle => 4,
            Self::AfterHandle => 5,
            Self::OnResponse => 6,
            Self::AfterResponse => 7,
        }
    }

    /// The phase name used by the Python API and diagnostics.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OnRequest => "on_request",
            Self::BeforeRoute => "before_route",
            Self::Guard => "guard",
            Self::Resolve => "resolve",
            Self::BeforeHandle => "before_handle",
            Self::AfterHandle => "after_handle",
            Self::OnResponse => "on_response",
            Self::AfterResponse => "after_response",
        }
    }

    /// The phase for `index`, or `None` when out of range.
    pub fn from_index(index: u32) -> Option<Self> {
        Self::ALL.get(index as usize).copied()
    }

    /// `true` for the post-handler phases, whose hooks also see the
    /// in-flight response.
    pub fn takes_response(self) -> bool {
        matches!(
            self,
            Self::AfterHandle | Self::OnResponse | Self::AfterResponse
        )
    }
}

impl std::fmt::Display for Phase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}
