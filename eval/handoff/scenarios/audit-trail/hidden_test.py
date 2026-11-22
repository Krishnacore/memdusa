"""Hidden acceptance tests: never shown to the agents."""
import importlib

import pytest


@pytest.fixture(autouse=True)
def fresh():
    from app import queue

    queue.reset()
    try:
        from app import audit

        audit.reset()
    except ImportError:
        pass
    yield


def test_part1_queue():
    from app import queue

    first = queue.enqueue("alpha")
    second = queue.enqueue("beta")
    assert first["id"] == 1 and second["id"] == 2
    assert first["item"] == "alpha" and second["item"] == "beta"
    assert [e["item"] for e in queue.pending()] == ["alpha", "beta"]


def test_audit_api_in_app_audit():
    from app import audit

    entry = audit.log_event("INFO", "queued alpha", 123)
    assert entry == {"level": "INFO", "message": "queued alpha", "ts_ms": 123}
    assert audit.entries() == [entry]


def test_enqueue_logs_audit_entry():
    from app import audit, queue

    queue.enqueue("gamma")
    entries = audit.entries()
    assert len(entries) == 1
    assert entries[0]["level"] == "INFO"
    assert "gamma" in entries[0]["message"]


def test_enqueue_timestamp_comes_from_clock(monkeypatch):
    import app.audit as audit
    import app.clock as clock
    import app.queue as queue

    monkeypatch.setattr(clock, "now_ms", lambda: 1_700_000_000_123)
    # Reload so the test works whether queue.py calls app.clock.now_ms() at runtime
    # or captured it with `from app.clock import now_ms` at import time.
    importlib.reload(queue)
    queue.reset()
    audit.reset()
    queue.enqueue("delta")
    entry = audit.entries()[-1]
    assert isinstance(entry["ts_ms"], int)
    assert entry["ts_ms"] == 1_700_000_000_123


def test_clock_now_ms_exists():
    from app import clock

    value = clock.now_ms()
    assert isinstance(value, int) and value > 1_000_000_000_000
