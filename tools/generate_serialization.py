#!/usr/bin/env python3
"""Generates crates/duckrs-core/src/storage/serialization/generated.rs.

The counterpart of DuckDB's scripts/generate_serialization.py: it reads the same JSON specs, and the
enums in the C++ headers, and writes readers that follow them field by field, so that the file
format stays in step with upstream mechanically.

C++ deserializes into the classes of its parser. The AST of duckrs has a shape of its own, so this
writes structs that mirror the specs, and `serialization/convert.rs` and `to_stored.rs` turn those
into the AST and back by hand. Objects are read as any version wrote them and written as v2.0.0.

    python3 tools/generate_serialization.py [--duckdb-root ../duckdb]
"""

import argparse
import json
import re
import subprocess
from pathlib import Path

SPECS = [
    "parsed_expression",
    "query_node",
    "result_modifier",
    "tableref",
    "statement",
    "constraint",
    "macro_function",
    "create_info",
    "nodes",
    "dependency",
    "storage",
    "wal",
]

# Where the generated readers start.
# The records of the WAL other than `WALCheckpoint`, which `wal.rs` reads, come from `wal.json`.
ROOTS = ["CreateInfo", "FunctionArgument", "IndexStorageInfo", "BlockPointer"]

# C++ deserializes these by hand. They are written by hand in `custom.rs` too.
CUSTOM = {"CastExpression", "FunctionExpression", "LambdaExpression", "WindowExpression", "QualifiedName"}

# Subclasses that no reader is generated for: a file with one fails to load. Their fields can't be
# skipped, as nothing says how long an object is. Each is for a statement or a clause that the
# engine doesn't run from a stored object yet.
NOT_PORTED = {
    "PatternExpression": "MATCH_RECOGNIZE patterns",
    "MatchRecognizeRef": "MATCH_RECOGNIZE",
    "ColumnDataRef": "column data references",
    "ShowRef": "SHOW and DESCRIBE in a stored query",
    "UpdateQueryNode": "UPDATE in a stored query",
    "DeleteQueryNode": "DELETE in a stored query",
    "InsertQueryNode": "INSERT in a stored query",
    "MergeQueryNode": "MERGE in a stored query",
    "CopyQueryNode": "COPY in a stored query",
    "CreateTriggerInfo": "triggers",
}

# Enums that convert.rs needs on top of those that select a subclass.
ENUMS = [
    "AggregateHandling",
    "CTEMaterialize",
    "CatalogType",
    "ConstraintCheckMode",
    "ExpressionType",
    "ForeignKeyType",
    "IndexConstraintType",
    "JoinRefType",
    "JoinType",
    "LambdaSyntaxType",
    "LimitValueType",
    "LiteralKind",
    "OnCreateConflict",
    "OrderByNullType",
    "OrderType",
    "OrdinalityType",
    "SampleMethod",
    "SetOperationType",
    "SubqueryType",
    "TableColumnType",
    "VectorType",
    "WALType",
    "WindowBoundary",
    "WindowExcludeMode",
]

KEYWORDS = {
    "as",
    "box",
    "enum",
    "final",
    "fn",
    "in",
    "macro",
    "match",
    "move",
    "ref",
    "struct",
    "type",
    "use",
    "where",
}

# Types that are a plain number in the file.
NUMBERS = {
    "bool": "bool",
    "int8_t": "i8",
    "int16_t": "i16",
    "int32_t": "i32",
    "int64_t": "i64",
    "uint8_t": "u8",
    "uint16_t": "u16",
    "uint32_t": "u32",
    "uint64_t": "u64",
    "idx_t": "u64",
    "block_id_t": "i64",
    "column_t": "u64",
    "double": "f64",
    "float": "f32",
    "optional_idx": "u64",
    "LogicalIndex": "u64",
    "PhysicalIndex": "u64",
}

ZEROABLE = {"bool", "int8_t", "int16_t", "int32_t", "int64_t", "uint8_t", "uint16_t", "uint32_t", "uint64_t", "idx_t"}


def split_args(text):
    """The arguments of a template, split at the commas outside of nested templates."""
    args, depth, current = [], 0, ""
    for char in text:
        if char == "," and depth == 0:
            args.append(current.strip())
            current = ""
            continue
        depth += char == "<"
        depth -= char == ">"
        current += char
    if current.strip():
        args.append(current.strip())
    return args


def template(text):
    """`name<args>` as (name, [args]), and (text, None) for anything else."""
    match = re.fullmatch(r"([\w:]+)<(.*)>", text)
    return (match[1], split_args(match[2])) if match else (text, None)


class Generator:
    def __init__(self, root):
        self.root = root
        self.spec_dir = root / "src/include/duckdb/storage/serialization"
        self.classes = {}
        self.bases = {}
        for spec in SPECS:
            for entry in json.loads((self.spec_dir / f"{spec}.json").read_text()):
                self.classes[entry["class"]] = entry
                if "base" in entry:
                    self.bases.setdefault(entry["base"], []).append(entry)
        self.enums_used = set(ENUMS)
        self.seen = []

    def is_pointer(self, ty):
        return ty.endswith("*") or ty.startswith("unique_ptr<") or ty.startswith("shared_ptr<")

    def rust_type(self, ty, base=None):
        ty = ty.replace("duckdb::", "").strip()
        if ty.endswith("*"):
            return f"Option<Box<{self.rust_type(base or ty[:-1])}>>"
        name, args = template(ty)
        if name in ("unique_ptr", "shared_ptr"):
            return f"Option<Box<{self.rust_type(args[0])}>>"
        if name == "vector":
            return f"Vec<{self.rust_type(args[0])}>"
        if name == "optional":
            return f"Option<{self.rust_type(args[0])}>"
        if name in ("identifier_map_t", "case_insensitive_map_t"):
            return f"Vec<(String, {self.rust_type(args[0])})>"
        if name == "InsertionOrderPreservingMap":
            key = self.rust_type(args[1]) if len(args) > 1 else "String"
            return f"Vec<({key}, {self.rust_type(args[0])})>"
        if name == "qualified_column_map_t":
            self.visit("QualifiedColumnName")
            return f"Vec<(QualifiedColumnName, {self.rust_type(args[0])})>"
        if name == "identifier_set_t":
            return "Vec<String>"
        if name == "qualified_column_set_t":
            self.visit("QualifiedColumnName")
            return "Vec<QualifiedColumnName>"
        if name == "create_info_set_t":
            self.visit("LogicalDependency")
            return "Vec<LogicalDependency>"
        if name == "GroupingSet":
            return "Vec<u64>"
        if name in ("string", "Identifier"):
            return "String"
        if name in NUMBERS:
            return NUMBERS[name]
        if name in ("Value", "LogicalType"):
            return name
        if name in CUSTOM or name in self.classes:
            self.visit(name)
            return name
        # everything else is an enum, which is a number in a binary file
        return "u64"

    def visit(self, name):
        """Queues a class to generate."""
        if name not in self.seen and name not in CUSTOM:
            self.seen.append(name)

    @staticmethod
    def field_name(member):
        name = member["name"]
        return f"{name}_" if name in KEYWORDS else name

    def member_type(self, member):
        return self.rust_type(member["type"], member.get("base"))

    @staticmethod
    def has_explicit_default(member):
        return "default" in member or "deserialize_default" in member or "required_until" in member

    def has_natural_default(self, member):
        ty = member["type"].replace("duckdb::", "")
        if ty == "identifier_set_t" or ty in ("string", "Identifier") or ty in ZEROABLE:
            return True
        return self.is_pointer(ty) or ("<" in ty and "IndexVector" not in ty)

    def is_option(self, member):
        """Whether a field is `None` when absent, for convert.rs to fill in its default. An empty
        container or a null pointer can stand for that; a default of `true` or 1 can't be zero."""
        ty = member["type"].replace("duckdb::", "")
        return self.has_explicit_default(member) and not (self.is_pointer(ty) or "<" in ty or ty == "identifier_set_t")

    def field_decl(self, member):
        if member.get("status") == "deleted":
            return None
        ty = self.member_type(member)
        if self.is_option(member):
            ty = f"Option<{ty}>"
        return f"    pub {self.field_name(member)}: {ty},\n"

    def field_read(self, member):
        ty = self.member_type(member)
        field, tag = member["id"], member["name"]
        if member.get("status") == "deleted":
            return f'        d.read_deleted_property::<{ty}>({field}, "{tag}")?;\n'
        name = self.field_name(member)
        if self.is_option(member):
            return f'        let {name} = d.read_optional_property::<{ty}>({field}, "{tag}")?;\n'
        if self.has_natural_default(member):
            return f'        let {name} = d.read_property_with_default::<{ty}>({field}, "{tag}")?;\n'
        return f'        let {name} = d.read_property::<{ty}>({field}, "{tag}")?;\n'

    @staticmethod
    def discriminant(entry):
        """The member of a base class that says which subclass follows."""
        class_type = entry["class_type"]
        return next(m for m in entry["members"] if class_type in (m["name"], m.get("property")))

    def field_write(self, member):
        """The write of a member as v2.0.0 has it. Fields of older versions stay out."""
        if member.get("status") in ("deleted", "read_only") or member.get("serialize_until") == "v2.0.0":
            return ""
        name = self.field_name(member)
        field, tag = member["id"], member["name"]
        if self.is_option(member):
            return f'        if let Some(value) = &self.{name} {{ s.write_property({field}, "{tag}", value); }}\n'
        if self.has_natural_default(member):
            return f'        s.write_property_with_default({field}, "{tag}", &self.{name});\n'
        return f'        s.write_property({field}, "{tag}", &self.{name});\n'

    def generate_plain(self, entry, members):
        """The struct of a class, and `read_members`, for its properties without the object around them."""
        name = entry["class"]
        out = f"#[derive(Debug, Clone)]\npub struct {name} {{\n"
        out += "".join(filter(None, map(self.field_decl, members)))
        out += "}\n\n"
        out += f"impl {name} {{\n    pub fn read_members(d: &mut BinaryDeserializer<'_>) -> Result<Self> {{\n"
        out += "".join(map(self.field_read, members))
        fields = ", ".join(self.field_name(m) for m in members if m.get("status") != "deleted")
        out += f"        Ok(Self {{ {fields} }})\n    }}\n\n"
        out += "    pub fn write_members(&self, s: &mut BinarySerializer) {\n"
        out += "".join(map(self.field_write, members))
        out += "    }\n}\n\n"
        return out

    @staticmethod
    def object_impl(name):
        return (
            f"impl Deserialize for {name} {{\n"
            "    fn read_value(d: &mut BinaryDeserializer<'_>) -> Result<Self> {\n"
            "        d.read_object_value(Self::read_members)\n"
            "    }\n}\n\n"
            f"impl Serialize for {name} {{\n"
            "    fn write_value(&self, s: &mut BinarySerializer) {\n"
            "        s.write_object_value(|s| self.write_members(s));\n"
            "    }\n}\n\n"
        )

    def generate_base(self, entry):
        name = entry["class"]
        members = entry["members"]
        discriminant = self.discriminant(entry)
        enum_type = discriminant["type"]
        self.enums_used.add(enum_type)
        module = snake(enum_type)
        variants, arms = "", ""
        for sub in self.bases[name]:
            sub_name = sub["class"]
            values = sub["enum"] if isinstance(sub["enum"], list) else [sub["enum"]]
            patterns = " | ".join(f"enums::{module}::{value}" for value in values)
            if sub_name in NOT_PORTED:
                arms += f'            {patterns} => return Err(not_ported("{NOT_PORTED[sub_name]}")),\n'
                continue
            variant = sub_name
            variants += f"    {variant}({sub_name}),\n"
            arms += f"            {patterns} => {name}Kind::{variant}({sub_name}::read_members(d)?),\n"
            self.visit(sub_name)
        out = f"#[derive(Debug, Clone)]\npub struct {name} {{\n"
        out += "".join(filter(None, map(self.field_decl, members)))
        out += f"    pub kind: {name}Kind,\n}}\n\n"
        out += f"#[derive(Debug, Clone)]\npub enum {name}Kind {{\n{variants}}}\n\n"
        out += f"impl {name} {{\n    pub fn read_members(d: &mut BinaryDeserializer<'_>) -> Result<Self> {{\n"
        out += "".join(map(self.field_read, members))
        out += f"        let kind = match u64::from({self.field_name(discriminant)}) {{\n{arms}"
        out += f'            other => return Err(unknown("{name}", other)),\n        }};\n'
        fields = ", ".join(self.field_name(m) for m in members if m.get("status") != "deleted")
        out += f"        Ok(Self {{ {fields}, kind }})\n    }}\n\n"
        out += "    pub fn write_members(&self, s: &mut BinarySerializer) {\n"
        out += "".join(map(self.field_write, members))
        out += "        match &self.kind {\n"
        for sub in self.bases[name]:
            if sub["class"] not in NOT_PORTED:
                out += f"            {name}Kind::{sub['class']}(value) => value.write_members(s),\n"
        out += "        }\n    }\n}\n\n"
        return out + self.object_impl(name)

    def generate_class(self, name):
        entry = self.classes[name]
        if name in self.bases:
            return self.generate_base(entry)
        out = self.generate_plain(entry, entry.get("members", []))
        return out if "base" in entry else out + self.object_impl(name)

    def enum_values(self):
        """Every `enum class` in the headers, as {name: [(member, value)]}."""
        found = {}
        for header in (self.root / "src/include").rglob("*.hpp"):
            text = re.sub(r"//.*", "", header.read_text(errors="replace"))
            text = re.sub(r"/\*.*?\*/", "", text, flags=re.S)
            for match in re.finditer(r"enum class (\w+)\s*(?::\s*\w+\s*)?\{(.*?)\}", text, re.S):
                if match[1] not in self.enums_used:
                    continue
                values, next_value = [], 0
                for item in match[2].split(","):
                    if not item.strip():
                        continue
                    left, _, right = item.partition("=")
                    member = left.strip()
                    if right.strip():
                        known = dict(values)
                        value = right.strip()
                        next_value = known[value] if value in known else int(value, 0)
                    values.append((member, next_value))
                    next_value += 1
                found[match[1]] = values
        return found

    def generate(self):
        body = ""
        records = json.loads((self.spec_dir / "wal.json").read_text())
        for root in ROOTS + [r["class"] for r in records if r["class"] != "WALCheckpoint"]:
            self.visit(root)
        index = 0
        while index < len(self.seen):
            body += self.generate_class(self.seen[index])
            index += 1
        values = self.enum_values()
        enums = "pub mod enums {\n"
        for name in sorted(self.enums_used):
            enums += f"    pub mod {snake(name)} {{\n"
            for member, value in values[name]:
                enums += f"        pub const {member}: u64 = {value};\n"
            enums += "    }\n"
        enums += "}\n\n"
        return HEADER + enums + body


HEADER = """\
// @generated by tools/generate_serialization.py from
// duckdb/src/include/duckdb/storage/serialization/*.json @ rust-port-baseline. Do not edit by hand.

#![allow(non_upper_case_globals, dead_code, unused_variables, clippy::all)]

use super::custom::{CastExpression, FunctionExpression, LambdaExpression, QualifiedName, WindowExpression};
use super::{not_ported, unknown};
use duckrs_common::Result;
use duckrs_common::serializer::{BinaryDeserializer, BinarySerializer, Deserialize, Serialize};
use duckrs_common::types::LogicalType;
use duckrs_common::value::Value;

"""


def snake(name):
    name = re.sub(r"(?<=[a-z0-9])(?=[A-Z])", "_", name)
    return re.sub(r"(?<=[A-Z])(?=[A-Z][a-z])", "_", name).lower()


def main():
    tools = Path(__file__).resolve().parent
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--duckdb-root", type=Path, default=tools.parent.parent / "duckdb")
    args = parser.parse_args()
    target = tools.parent / "crates/duckrs-core/src/storage/serialization/generated.rs"
    target.write_text(Generator(args.duckdb_root).generate())
    subprocess.run(["rustfmt", "--edition", "2024", str(target)], check=True)
    print(f"wrote {target}")


if __name__ == "__main__":
    main()
