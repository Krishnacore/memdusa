from app import registry


def setup_function():
    registry.reset()


def test_register_and_get():
    registry.register("a", 5)
    assert registry.get("a") == 5
    assert registry.all_names() == ["a"]
