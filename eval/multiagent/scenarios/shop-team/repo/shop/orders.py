"""Orders for the shop. All amounts are integer cents."""

ORDERS = {1: 5000, 2: 1250}


def get_order(order_id: int) -> int:
    try:
        return ORDERS[order_id]
    except KeyError:
        raise KeyError(f"unknown order {order_id}") from None


def reset() -> None:
    ORDERS.clear()
    ORDERS.update({1: 5000, 2: 1250})
