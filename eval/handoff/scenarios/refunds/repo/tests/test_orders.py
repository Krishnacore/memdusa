from shop import orders
from shop.orders import Item


def setup_function():
    orders.reset()


def test_order_total():
    o = orders.create_order("o1", [Item("a", 250, 2), Item("b", 100)])
    assert o.total_cents == 600
