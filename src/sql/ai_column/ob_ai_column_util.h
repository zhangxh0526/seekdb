/*
 * Copyright (c) 2025 OceanBase.
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

#pragma once

#include "lib/container/ob_se_array.h"
#include "lib/string/ob_sql_string.h"
#include "share/schema/ob_table_schema.h"
#include "sql/parser/parse_node.h"

namespace oceanbase
{
namespace sql
{

enum ObAIColumnState
{
  AI_COLUMN_READY = 0,
  AI_COLUMN_PENDING = 1,
  AI_COLUMN_RUNNING = 2,
  AI_COLUMN_RETRY = 3,
  AI_COLUMN_FAILED = 4
};

struct ObAIColumnMetadata
{
  ObAIColumnMetadata() : function_type_(T_INVALID), max_retries_(0), dependency_column_ids_() {}
  ObItemType function_type_;
  common::ObString model_name_;
  common::ObString normalized_expr_;
  common::ObString prompt_expr_;
  common::ObString system_prompt_;
  common::ObString sync_mode_;
  int64_t max_retries_;
  common::ObSEArray<uint64_t, 8> dependency_column_ids_;
};

class ObAIColumnUtil
{
public:
  static bool is_ai_column(const share::schema::ObColumnSchemaV2 &column);
  static bool is_ai_column(const share::schema::ObTableSchema &table);
  static int get_hidden_column_name(common::ObIAllocator &allocator,
                                    const common::ObString &result_column_name,
                                    const char *suffix,
                                    common::ObString &hidden_column_name);
  static int get_state_column_name(common::ObIAllocator &allocator,
                                   const common::ObString &result_column_name,
                                   common::ObString &hidden_column_name);
  static int get_request_column_name(common::ObIAllocator &allocator,
                                     const common::ObString &result_column_name,
                                     common::ObString &hidden_column_name);
  static int get_result_spec_column_name(common::ObIAllocator &allocator,
                                         const common::ObString &result_column_name,
                                         common::ObString &hidden_column_name);
  static int resolve_ai_column_definition(share::schema::ObColumnSchemaV2 &column,
                                          ParseNode &column_definition,
                                          const common::ObIArray<share::schema::ObColumnSchemaV2> &resolved_columns,
                                          common::ObIAllocator &allocator);
  static int make_hidden_column(const share::schema::ObColumnSchemaV2 &result_column,
                                const int64_t hidden_index,
                                share::schema::ObColumnSchemaV2 &hidden_column);
  static int parse_metadata(common::ObIAllocator &allocator,
                            const share::schema::ObColumnSchemaV2 &column,
                            ObAIColumnMetadata &metadata);
  static int append_json_string(common::ObSqlString &json, const common::ObString &value);
private:
  static int collect_dependencies(const ParseNode *node,
                                  const common::ObIArray<share::schema::ObColumnSchemaV2> &resolved_columns,
                                  ObAIColumnMetadata &metadata);
  static int extract_prompt_expr(const common::ObString &normalized_expr,
                                 common::ObString &prompt_expr);
};

} // namespace sql
} // namespace oceanbase
