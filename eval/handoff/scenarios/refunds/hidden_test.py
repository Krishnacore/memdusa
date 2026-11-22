"""Hidden acceptance tests: never shown to the agents."""
import subprocess
import sys

import pytest

from shop import ledger, orders
from shop.orders import Item


@pytest.fixture(autouse=True)
def fresh():
    orders.reset()
    ledger.reset()
    orders.create_order("o1", [Item("a", 1000)])
    yield


def refunds():
    from shop import refunds as r

    return r


def test_partial_refunds_and_helpers():
    r = refunds()
    r.refund("o1", 300, "Damaged box")
    r.refund("o1", 200, "late")
    assert r.refunded_cents("o1") == 500
    assert r.refundable_cents("o1") == 500


def test_refund_recorded_in_ledger_with_lowercased_reason():
    r = refunds()
    entry = r.refund("o1", 100, "Damaged BOX")
    assert entry.kind == "refund" and entry.amount_cents == 100
    assert entry.note == "damaged box"
    assert [e.kind for e in ledger.entries("o1")] == ["refund"]


def test_refund_error_is_value_error():
    assert issubclass(refunds().RefundError, ValueError)


@pytest.mark.parametrize(
    "order_id, amount, reason",
    [("o1", 0, "x"), ("o1", -5, "x"), ("o1", 1001, "x"), ("nope", 10, "x"), ("o1", 10, ""), ("o1", 10, "   ")],
)
def test_invalid_refunds_raise(order_id, amount, reason):
    r = refunds()
    with pytest.raises(r.RefundError):
        r.refund(order_id, amount, reason)


def test_cannot_exceed_total_cumulatively():
    r = refunds()
    r.refund("o1", 900, "a")
    with pytest.raises(r.RefundError):
        r.refund("o1", 101, "b")


def test_cli_refund_prints_remaining(capsys):
    from shop import cli

    assert cli.main(["refund", "o1", "250", "--reason", "Broken"]) == 0
    assert capsys.readouterr().out.strip() == "750"


def test_amounts_are_ints():
    r = refunds()
    r.refund("o1", 100, "x")
    assert isinstance(r.refundable_cents("o1"), int)
