"""Errors raised for violations of the yesnodb protocol."""


class YesnoError(Exception):
    """Base class for client-side yesnodb errors."""


class ProtocolError(YesnoError):
    """The server returned a response that violates the yesnodb protocol."""


class ExpressionError(ValueError, YesnoError):
    """A set expression is invalid or has a malformed wire representation."""


class TicketError(ValueError, YesnoError):
    """A query ticket has a malformed wire representation."""
