import importlib.metadata

import breezed


def test_version_matches_package_metadata():
    assert breezed.__version__ == importlib.metadata.version("breezed")
