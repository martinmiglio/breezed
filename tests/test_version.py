import breezed


def test_version_is_nonempty_string():
    assert isinstance(breezed.__version__, str)
    assert breezed.__version__
