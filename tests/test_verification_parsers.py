from __future__ import annotations

from singularity.verification.parsers import MAX_PARSED_FAILURES, PytestFailureParser


def test_pytest_failure_parser_caps_unique_failures_at_named_limit() -> None:
    parser = PytestFailureParser()
    output = "\n".join(
        f"FAILED tests/test_case_{index}.py::test_case_{index} - AssertionError: boom {index}"
        for index in range(MAX_PARSED_FAILURES + 5)
    )

    parsed = parser.parse(output)

    assert len(parsed) == MAX_PARSED_FAILURES
    assert parsed[0].test_name == "test_case_0"
    assert parsed[-1].test_name == f"test_case_{MAX_PARSED_FAILURES - 1}"


def test_pytest_failure_parser_dedupes_before_limit() -> None:
    parser = PytestFailureParser()
    repeated = "FAILED tests/test_repeat.py::test_repeat - AssertionError: boom"
    unique_lines = [
        f"FAILED tests/test_case_{index}.py::test_case_{index} - AssertionError: boom {index}"
        for index in range(MAX_PARSED_FAILURES - 1)
    ]
    output = "\n".join([repeated, repeated, *unique_lines])

    parsed = parser.parse(output)

    assert len(parsed) == MAX_PARSED_FAILURES
    assert [failure.test_name for failure in parsed].count("test_repeat") == 1
