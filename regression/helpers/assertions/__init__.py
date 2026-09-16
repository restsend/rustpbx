"""Strict assertion toolkit for the unified regression system.

Modules:
    guards        -- fail-fast require/require_non_empty/require_file/require_keys + diff_map
    audio_assert  -- AudioCheckResult + assert_audio / assert_stereo_split
    cdr_schema    -- CdrCheckResult + assert_cdr (schema/enum/time-chain/fields)
    event_schema  -- RWI/webhook event validation + ordered flow matching
    sip_assert    -- exact SIP status sequences + exact DTMF digits
"""

from .guards import (  # noqa: F401
    require,
    require_non_empty,
    require_file,
    require_keys,
    forbid,
    diff_map,
)
from .audio_assert import AudioCheckResult, StereoSplitResult, assert_audio, assert_stereo_split, assert_caller_audio  # noqa: F401
from .cdr_schema import (  # noqa: F401
    CdrCheckResult,
    HANGUP_REASONS,
    assert_cdr,
    load_cdr,
    unwrap_cdr,
)
from .event_schema import (  # noqa: F401
    CALL_SCOPED,
    LIFECYCLE_ORDER,
    TYPE_EXTRAS,
    assert_event_flow,
    assert_no_ghost_events,
    event_call_id,
    event_type,
    validate_event,
)
from .sip_assert import (  # noqa: F401
    assert_dtmf_digits,
    assert_sip_answered,
    assert_sip_no_answer,
    assert_sip_rejected,
    codes_summary,
)
