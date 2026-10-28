#!/usr/bin/env python3
"""Unchanged baseline assertions applicable to the opt-in in-process entry."""
import importlib.util
import inspect
from pathlib import Path

root = Path(__file__).resolve().parents[5]
spec = importlib.util.spec_from_file_location('baseline', root / 'apps/light-lambda/tests/l2/run.py')
baseline = importlib.util.module_from_spec(spec)
spec.loader.exec_module(baseline)
source = inspect.getsource(baseline.main)
needle = "('validator','enabled handler not supported'),"
assert source.count(needle) == 1
# This executable implements Validator. Preserve the ordinary-bootstrap
# unavailable expectation in run.py; retain every other baseline assertion.
source = source.replace(needle, '')
exec(source, baseline.__dict__)
if __name__ == '__main__':
    baseline.main()
