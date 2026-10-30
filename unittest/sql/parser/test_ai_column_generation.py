#!/usr/bin/env python3
"""Check SQL grammar generation without configuring or compiling seekdb.

Run with the pinned dependency toolchain, or set SEEKDB_TEST_BISON to GNU
Bison 2.4.1. The repository's parser generator rejects conflict diagnostics
even when Bison itself exits successfully.
"""

import os
from pathlib import Path
import subprocess
import tempfile
import unittest


class AIColumnGrammarGenerationTest(unittest.TestCase):
    def test_sql_grammar_is_conflict_free(self):
        repo = Path(__file__).resolve().parents[3]
        bundled = repo / "deps/3rd/usr/local/oceanbase/devtools/bin/bison"
        bison = os.environ.get("SEEKDB_TEST_BISON", str(bundled) if bundled.is_file() else "bison")
        version = subprocess.check_output([bison, "--version"], text=True).splitlines()[0]
        self.assertEqual(version, "bison (GNU Bison) 2.4.1", "Use seekdb's pinned Bison version")
        grammar = repo / "src/sql/parser/sql_parser_mysql_mode.y"
        with tempfile.TemporaryDirectory(prefix="seekdb-sql-grammar-") as output:
            generated = Path(output) / "sql_parser_mysql_mode_tab.c"
            result = subprocess.run(
                [bison, "-Werror", "-d", str(grammar), "-o", str(generated)],
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                check=False,
            )
            self.assertEqual(result.returncode, 0, result.stdout)
            self.assertNotIn("conflict", result.stdout.lower(), result.stdout)
            self.assertTrue(generated.is_file())


if __name__ == "__main__":
    unittest.main()
