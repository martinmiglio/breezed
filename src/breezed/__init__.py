"""Curve-based fan controller for Dell PowerEdge servers via iDRAC/IPMI."""

try:
    from breezed._version import __version__
except ImportError:  # source tree without a build (rare)
    from importlib.metadata import version

    __version__ = version("breezed")
