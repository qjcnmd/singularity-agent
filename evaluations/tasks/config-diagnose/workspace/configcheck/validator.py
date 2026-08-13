"""配置校验：必填字段与类型检查。"""

from .loader import ConfigError, load_settings

REQUIRED_STRING_FIELDS = ("name",)
REQUIRED_INT_FIELDS = ("port",)


def validate(settings: dict) -> list[str]:
    """返回校验问题列表（空列表 = 校验通过）。"""
    problems = []
    for field in REQUIRED_STRING_FIELDS:
        value = settings.get(field)
        if not isinstance(value, str) or not value:
            problems.append(f"field {field!r} must be a non-empty string")
    for field in REQUIRED_INT_FIELDS:
        value = settings.get(field)
        if isinstance(value, bool) or not isinstance(value, int):
            problems.append(f"field {field!r} must be an integer")
    return problems


def run_validation() -> list[str]:
    """加载并校验配置，返回问题列表。"""
    return validate(load_settings())
