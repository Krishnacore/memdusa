from shop import orders


def setup_function():
    orders.reset()


def test_order_total():
    assert orders.get_order(1) == 5000
    assert orders.get_order(2) == 1250
