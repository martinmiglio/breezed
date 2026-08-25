import importlib.metadata

import breezed


def test_version_is_nonempty_string():
    assert isinstance(breezed.__version__, str)
    assert breezed.__version__


def test_version_matches_package_metadata():
    assert breezed.__version__ == importlib.metadata.version("breezed")
