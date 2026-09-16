"""Acceptance test conftest.

Reuses fixtures from the root conftest: pbx, api, sipbot_pool, event_checker, rwi, webhook_session.
Tests target real SIP calls + webhook event verification. Mocks/stubs are not
permitted; where a prerequisite (e.g. PhoneAuth JWT) is unavailable the test
should pytest.skip with a clear reason rather than swallow the assertion.
"""
