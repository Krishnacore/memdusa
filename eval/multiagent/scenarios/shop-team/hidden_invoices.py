"""Hidden acceptance tests for the `invoices` task. Never shown to the agents."""
from pathlib import Path

from shop.orders import ORDERS


def setup_function():
    ORDERS.clear()
    ORDERS.update({1: 5000, 2: 1250})


def test_render_invoice_format():
    from shop import invoices as inv
    assert inv.render_invoice(1) == "INVOICE #1 TOTAL 5000 cents"
    assert inv.render_invoice(2) == "INVOICE #2 TOTAL 1250 cents"


def test_invoice_lines():
    from shop import invoices as inv
    assert inv.invoice_lines(1) == ["INVOICE #1", "TOTAL 5000 cents"]


def test_invoices_tests_file_exists():
    assert Path("tests/test_invoices.py").is_file()
