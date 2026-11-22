"""Hidden acceptance tests for the `refunds` task. Never shown to the agents."""
import subprocess
import sys
from pathlib import Path

import pytest

from shop import ledger, orders
from shop.orders import ORDERS


@pytest.fixture(autouse=True)
def fresh():
    ledger.reset()
    ORDERS.clear()
    ORDERS.update({1: 5000, 2: 1250})
    yield


def test_refunds_module_exists():
    assert Path("shop/refunds.py").is_file()


def test_tests_file_exists_and_passes():
    p = Path("tests/test_refunds.py")
    assert p.is_file(), "the worker must add its own tests/test_refunds.py"
    r = subprocess.run([sys.executable, "-m", "pytest", "-q", "--tb=short", str(p)],
                       capture_output=True, text=True)
    assert r.returncode == 0, r.stdout + r.stderr


def test_partial_refund_and_lowercased_reason():
    from shop import refunds as r
    entry = r.refund(1, 300, "Damaged BOX")
    assert entry.kind == "refund"
    assert entry.amount_cents == 300
    assert entry.note == "damaged box"
    assert r.refunded_cents(1) == 300
    assert r.refundable_cents(1) == 4700


def test_over_refund_raises_value_error():
    from shop import refunds as r
    with pytest.raises(ValueError):
        r.refund(1, 5001, "too much")
    r.refund(2, 1000, "ok")
    with pytest.raises(ValueError):
        r.refund(2, 300, "too much now")


def test_invalid_refunds_raise():
    from shop import refunds as r
    assert issubclass(r.RefundError, ValueError)
    for order_id, amount, reason in [(1, 0, "x"), (1, -5, "x"), (999, 10, "x"), (1, 10, "  ")]:
        with pytest.raises(r.RefundError):
            r.refund(order_id, amount, reason)
