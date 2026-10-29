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

#define USING_LOG_PREFIX SQL_RESV
#include "sql/ai_column/ob_ai_column_util.h"
#include "common/json_type/ob_json_base.h"
#include "common/json_type/ob_json_tree.h"
#include "lib/ob_errno.h"
#include "lib/utility/ob_macro_utils.h"
#include "share/schema/ob_schema_struct.h"
#include "share/schema/ob_column_schema.h"
#include <algorithm>
#include <cctype>

namespace oceanbase
{
namespace sql
{
using namespace common;
using namespace share::schema;

namespace
{
constexpr const char *AI_STATE_SUFFIX = "_ai_state";
constexpr const char *AI_REQUEST_SUFFIX = "_ai_request";
constexpr const char *AI_RESULT_SPEC_SUFFIX = "_ai_result_spec";
constexpr const char *AI_JSON_PROMPT =
    "Return exactly one valid JSON value. Do not use markdown fences or add commentary.";
constexpr const char *AI_INTEGER_PROMPT =
    "Return exactly one base-10 integer. Do not use markdown fences or add commentary.";

int get_json_string(ObJsonObject *object, const char *key, ObString &value)
{
  int ret = OB_SUCCESS;
  ObJsonNode *node = nullptr;
  if (OB_ISNULL(object) || OB_ISNULL(key)) {
    ret = OB_INVALID_ARGUMENT;
  } else if (OB_ISNULL(node = object->get_value(key))) {
    ret = OB_INVALID_DATA;
  } else if (node->json_type() != ObJsonNodeType::J_STRING) {
    ret = OB_INVALID_DATA;
  } else {
    value = static_cast<ObJsonString *>(node)->get_str();
  }
  return ret;
}
}

bool ObAIColumnUtil::is_ai_column(const ObColumnSchemaV2 &column)
{
  return column.has_column_flag(AI_COLUMN_FLAG);
}

bool ObAIColumnUtil::is_ai_column(const ObTableSchema &table)
{
  bool found = false;
  for (ObTableSchema::const_column_iterator it = table.column_begin();
       !found && it != table.column_end(); ++it) {
    found = OB_NOT_NULL(*it) && is_ai_column(**it);
  }
  return found;
}

int ObAIColumnUtil::get_hidden_column_name(ObIAllocator &allocator,
                                           const ObString &result_column_name,
                                           const char *suffix,
                                           ObString &hidden_column_name)
{
  int ret = OB_SUCCESS;
  const int64_t suffix_len = strlen(suffix);
  const int64_t max_prefix_len = OB_MAX_COLUMN_NAME_LENGTH - suffix_len - 2;
  const int64_t prefix_len = std::min(static_cast<int64_t>(result_column_name.length()), max_prefix_len);
  char *name = nullptr;
  if (OB_ISNULL(suffix) || prefix_len < 0) {
    ret = OB_INVALID_ARGUMENT;
  } else if (OB_ISNULL(name = static_cast<char *>(allocator.alloc(prefix_len + suffix_len + 2)))) {
    ret = OB_ALLOCATE_MEMORY_FAILED;
  } else {
    int64_t pos = 0;
    name[pos++] = '_';
    name[pos++] = '_';
    MEMCPY(name + pos, result_column_name.ptr(), prefix_len);
    pos += prefix_len;
    MEMCPY(name + pos, suffix, suffix_len);
    pos += suffix_len;
    hidden_column_name.assign_ptr(name, static_cast<ObString::obstr_size_t>(pos));
  }
  return ret;
}

int ObAIColumnUtil::get_state_column_name(ObIAllocator &allocator,
                                          const ObString &result_column_name,
                                          ObString &hidden_column_name)
{
  return get_hidden_column_name(allocator, result_column_name, AI_STATE_SUFFIX, hidden_column_name);
}

int ObAIColumnUtil::get_request_column_name(ObIAllocator &allocator,
                                            const ObString &result_column_name,
                                            ObString &hidden_column_name)
{
  return get_hidden_column_name(allocator, result_column_name, AI_REQUEST_SUFFIX, hidden_column_name);
}

int ObAIColumnUtil::get_result_spec_column_name(ObIAllocator &allocator,
                                                const ObString &result_column_name,
                                                ObString &hidden_column_name)
{
  return get_hidden_column_name(allocator, result_column_name, AI_RESULT_SPEC_SUFFIX, hidden_column_name);
}

int ObAIColumnUtil::append_json_string(ObSqlString &json, const ObString &value)
{
  int ret = OB_SUCCESS;
  if (OB_FAIL(json.append("\""))) {
  } else {
    for (int64_t i = 0; OB_SUCC(ret) && i < value.length(); ++i) {
      const char ch = value.ptr()[i];
      if (ch == '"' || ch == '\\') {
        ret = json.append_fmt("\\%c", ch);
      } else if (ch == '\n') {
        ret = json.append("\\n");
      } else if (ch == '\r') {
        ret = json.append("\\r");
      } else if (ch == '\t') {
        ret = json.append("\\t");
      } else if (static_cast<unsigned char>(ch) < 0x20) {
        ret = json.append_fmt("\\u%04x", static_cast<unsigned int>(ch));
      } else {
        ret = json.append(&value.ptr()[i], 1);
      }
    }
  }
  if (OB_SUCC(ret)) {
    ret = json.append("\"");
  }
  return ret;
}

int ObAIColumnUtil::extract_prompt_expr(const ObString &normalized_expr,
                                        ObString &prompt_expr)
{
  int ret = OB_SUCCESS;
  int64_t depth = 0;
  int64_t comma = -1;
  int64_t end = -1;
  bool in_string = false;
  for (int64_t i = 0; i < normalized_expr.length(); ++i) {
    const char ch = normalized_expr.ptr()[i];
    if (ch == '\'' && (i == 0 || normalized_expr.ptr()[i - 1] != '\\')) {
      in_string = !in_string;
    } else if (!in_string) {
      if (ch == '(') {
        ++depth;
      } else if (ch == ')') {
        if (depth == 1) {
          end = i;
          break;
        }
        --depth;
      } else if (ch == ',' && depth == 1 && comma < 0) {
        comma = i;
      }
    }
  }
  if (comma < 0 || end <= comma + 1) {
    ret = OB_INVALID_ARGUMENT;
  } else {
    int64_t begin = comma + 1;
    while (begin < end && isspace(normalized_expr.ptr()[begin])) {
      ++begin;
    }
    int64_t trimmed_end = end;
    while (trimmed_end > begin && isspace(normalized_expr.ptr()[trimmed_end - 1])) {
      --trimmed_end;
    }
    prompt_expr.assign_ptr(normalized_expr.ptr() + begin,
                           static_cast<ObString::obstr_size_t>(trimmed_end - begin));
  }
  return ret;
}

int ObAIColumnUtil::collect_dependencies(const ParseNode *node,
                                         const ObIArray<ObColumnSchemaV2> &resolved_columns,
                                         ObAIColumnMetadata &metadata)
{
  int ret = OB_SUCCESS;
  if (OB_ISNULL(node)) {
    return ret;
  } else if (node->type_ == T_COLUMN_REF && node->num_child_ == 3 && OB_NOT_NULL(node->children_[2])) {
    const ObString column_name(node->children_[2]->str_len_, node->children_[2]->str_value_);
    for (int64_t i = 0; OB_SUCC(ret) && i < resolved_columns.count(); ++i) {
      const ObColumnSchemaV2 &resolved = resolved_columns.at(i);
      if (0 == column_name.case_compare(resolved.get_column_name_str())) {
        if (is_ai_column(resolved)) {
          ret = OB_NOT_SUPPORTED;
        } else if (!has_exist_in_array(metadata.dependency_column_ids_, resolved.get_column_id())
                   && OB_FAIL(metadata.dependency_column_ids_.push_back(resolved.get_column_id()))) {
        }
        break;
      }
    }
  }
  for (int64_t i = 0; OB_SUCC(ret) && i < node->num_child_; ++i) {
    if (OB_FAIL(collect_dependencies(node->children_[i], resolved_columns, metadata))) {
    }
  }
  return ret;
}

int ObAIColumnUtil::resolve_ai_column_definition(ObColumnSchemaV2 &column,
                                                 ParseNode &column_definition,
                                                 const ObIArray<ObColumnSchemaV2> &resolved_columns,
                                                 ObIAllocator &allocator)
{
  int ret = OB_SUCCESS;
  ObAIColumnMetadata metadata;
  metadata.max_retries_ = 2;
  metadata.sync_mode_ = ObString::make_string("async");
  ParseNode *expr_node = column_definition.children_[3];
  ParseNode *arguments = nullptr;
  ObString function_name;
  if (column_definition.num_child_ == 7 && OB_NOT_NULL(expr_node)
      && expr_node->type_ == T_FUN_SYS && expr_node->num_child_ == 2
      && OB_NOT_NULL(expr_node->children_[0]) && OB_NOT_NULL(expr_node->children_[1])) {
    function_name.assign_ptr(expr_node->children_[0]->str_value_,
                             static_cast<ObString::obstr_size_t>(expr_node->children_[0]->str_len_));
    arguments = expr_node->children_[1];
    if (0 == function_name.case_compare("ai_complete")) {
      metadata.function_type_ = T_FUN_SYS_AI_COMPLETE;
    } else if (0 == function_name.case_compare("ai_embed")) {
      metadata.function_type_ = T_FUN_SYS_AI_EMBED;
    }
  }
  if (metadata.function_type_ == T_INVALID || OB_ISNULL(arguments)
      || arguments->type_ != T_EXPR_LIST || arguments->num_child_ < 2 || arguments->num_child_ > 3) {
    ret = OB_NOT_SUPPORTED;
    LOG_USER_ERROR(OB_NOT_SUPPORTED, "AI COLUMN expression must be AI_COMPLETE or AI_EMBED");
  } else if (metadata.function_type_ == T_FUN_SYS_AI_COMPLETE && !column.is_string_type() && !column.is_json()
             && !ob_is_integer_type(column.get_data_type())) {
    ret = OB_NOT_SUPPORTED;
    LOG_USER_ERROR(OB_NOT_SUPPORTED, "AI_COMPLETE result column must have a scalar type");
  } else if (metadata.function_type_ == T_FUN_SYS_AI_EMBED
             && (column.get_data_type() != ObCollectionSQLType
                 || column.get_extended_type_info().count() != 1
                 || !column.get_extended_type_info().at(0).prefix_match("VECTOR("))) {
    ret = OB_NOT_SUPPORTED;
    LOG_USER_ERROR(OB_NOT_SUPPORTED, "AI_EMBED result column must have a vector type");
  } else {
    metadata.model_name_.assign_ptr(arguments->children_[0]->str_value_,
                                    static_cast<ObString::obstr_size_t>(arguments->children_[0]->str_len_));
    metadata.normalized_expr_.assign_ptr(expr_node->str_value_,
                                         static_cast<ObString::obstr_size_t>(expr_node->str_len_));
    if (metadata.model_name_.empty() || metadata.normalized_expr_.empty()) {
      ret = OB_INVALID_ARGUMENT;
    }
  }
  if (OB_SUCC(ret) && metadata.function_type_ == T_FUN_SYS_AI_EMBED && arguments->num_child_ == 3) {
    int64_t declared_dim = 0;
    if (column.get_extended_type_info().count() == 1) {
      ObString type_info = column.get_extended_type_info().at(0);
      type_info.split_on('(').trim();
      ObString dimension = type_info.split_on(')').trim();
      declared_dim = atoi(dimension.ptr());
    }
    const int64_t requested_dim = arguments->children_[2]->value_;
    if (declared_dim > 0 && requested_dim != declared_dim) {
      ret = OB_ERR_INVALID_VECTOR_DIM;
      LOG_USER_ERROR(OB_ERR_INVALID_VECTOR_DIM, declared_dim, requested_dim);
    }
  }
  if (OB_SUCC(ret)) {
    if (column.is_json()) {
      metadata.system_prompt_ = ObString::make_string(AI_JSON_PROMPT);
    } else if (ob_is_integer_type(column.get_data_type())) {
      metadata.system_prompt_ = ObString::make_string(AI_INTEGER_PROMPT);
    }
    if (OB_FAIL(extract_prompt_expr(metadata.normalized_expr_, metadata.prompt_expr_))) {
    } else if (OB_FAIL(collect_dependencies(arguments->children_[1], resolved_columns, metadata))) {
    }
  }
  ParseNode *options = column_definition.children_[4];
  if (OB_NOT_NULL(options) && options->type_ == T_AI_COLUMN_OPTIONS && options->num_child_ == 1) {
    options = options->children_[0];
  }
  for (ParseNode *option = options; OB_SUCC(ret) && OB_NOT_NULL(option); ) {
    ParseNode *current = option;
    ParseNode *next = nullptr;
    if (option->type_ == T_LINK_NODE && option->num_child_ == 2) {
      current = option->children_[1];
      next = option->children_[0];
    }
    if (OB_ISNULL(current) || current->type_ != T_AI_COLUMN_OPTION || current->num_child_ != 2) {
      option = next;
      continue;
    }
    const ObString name(current->children_[0]->str_len_, current->children_[0]->str_value_);
    if (0 == name.case_compare("max_retries")) {
      metadata.max_retries_ = current->children_[1]->value_;
      if (metadata.max_retries_ < 0 || metadata.max_retries_ > 100) {
        ret = OB_INVALID_ARGUMENT;
      }
    } else if (0 == name.case_compare("sync_mode")) {
      metadata.sync_mode_.assign_ptr(current->children_[1]->str_value_,
                                     static_cast<ObString::obstr_size_t>(current->children_[1]->str_len_));
      if (0 != metadata.sync_mode_.case_compare("async")) {
        ret = OB_NOT_SUPPORTED;
        LOG_USER_ERROR(OB_NOT_SUPPORTED, "AI COLUMN sync_mode other than async");
      }
    }
    option = next;
  }
  ObSqlString normalized;
  ObSqlString json;
  if (OB_FAIL(ret)) {
  } else if (!metadata.system_prompt_.empty()
             && OB_FAIL(normalized.append_fmt("%.*s /* %.*s */",
                 metadata.normalized_expr_.length(), metadata.normalized_expr_.ptr(),
                 metadata.system_prompt_.length(), metadata.system_prompt_.ptr()))) {
  } else if (metadata.system_prompt_.empty()
             && OB_FAIL(normalized.assign(metadata.normalized_expr_))) {
  } else if (FALSE_IT(metadata.normalized_expr_.assign_ptr(normalized.ptr(), normalized.length()))) {
  } else if (OB_FAIL(json.append("{"))) {
  } else if (OB_FAIL(json.append("\"function\":"))) {
  } else if (OB_FAIL(append_json_string(json, ObString::make_string(
                 metadata.function_type_ == T_FUN_SYS_AI_COMPLETE ? "complete" : "embed")))) {
  } else if (OB_FAIL(json.append(",\"model\":")) || OB_FAIL(append_json_string(json, metadata.model_name_))
             || OB_FAIL(json.append(",\"normalized_expr\":")) || OB_FAIL(append_json_string(json, metadata.normalized_expr_))
             || OB_FAIL(json.append(",\"prompt_expr\":")) || OB_FAIL(append_json_string(json, metadata.prompt_expr_))
             || OB_FAIL(json.append(",\"system_prompt\":")) || OB_FAIL(append_json_string(json, metadata.system_prompt_))
             || OB_FAIL(json.append(",\"sync_mode\":")) || OB_FAIL(append_json_string(json, metadata.sync_mode_))
             || OB_FAIL(json.append_fmt(",\"max_retries\":%ld,\"dependencies\":[", metadata.max_retries_))) {
  } else {
    for (int64_t i = 0; OB_SUCC(ret) && i < metadata.dependency_column_ids_.count(); ++i) {
      if (i > 0 && OB_FAIL(json.append(","))) {
      }
      if (OB_FAIL(ret)) {
      } else if (OB_FAIL(json.append_fmt("%lu", metadata.dependency_column_ids_.at(i)))) {
      }
    }
    if (OB_SUCC(ret)) {
      ret = json.append("]}");
    }
  }
  ObObj null_value;
  null_value.set_null();
  if (OB_FAIL(ret)) {
  } else if (OB_FAIL(column.set_cur_default_value(null_value, false))) {
  } else if (OB_FAIL(column.set_comment(ObString(json.length(), json.ptr())))) {
  } else {
    column.add_column_flag(AI_COLUMN_FLAG);
    column.set_nullable(true);
  }
  return ret;
}

int ObAIColumnUtil::make_hidden_column(const ObColumnSchemaV2 &result_column,
                                       const int64_t hidden_index,
                                       ObColumnSchemaV2 &hidden_column)
{
  int ret = OB_SUCCESS;
  ObString name;
  ObObj default_value;
  ObArenaAllocator allocator;
  ObAIColumnMetadata metadata;
  hidden_column.reset();
  hidden_column.set_table_id(result_column.get_table_id());
  hidden_column.set_column_id(OB_INVALID_ID);
  hidden_column.set_is_hidden(true);
  hidden_column.set_rowkey_position(0);
  hidden_column.set_index_position(0);
  hidden_column.set_charset_type(CHARSET_UTF8MB4);
  hidden_column.set_collation_type(CS_TYPE_UTF8MB4_BIN);
  if (hidden_index == 0) {
    hidden_column.set_data_type(ObIntType);
    hidden_column.set_nullable(false);
    default_value.set_int(AI_COLUMN_PENDING);
    if (OB_FAIL(get_state_column_name(allocator, result_column.get_column_name_str(), name))) {
    }
  } else if (hidden_index == 1) {
    hidden_column.set_data_type(ObUInt64Type);
    hidden_column.set_nullable(false);
    default_value.set_uint64(0);
    if (OB_FAIL(get_request_column_name(allocator, result_column.get_column_name_str(), name))) {
    }
  } else if (hidden_index == 2) {
    hidden_column.set_data_type(ObVarcharType);
    hidden_column.set_nullable(false);
    if (OB_FAIL(parse_metadata(allocator, result_column, metadata))) {
    } else {
      hidden_column.set_data_length(std::max<int64_t>(1, metadata.normalized_expr_.length()));
      default_value.set_varchar(metadata.normalized_expr_);
      default_value.set_collation_type(CS_TYPE_UTF8MB4_BIN);
    }
    if (OB_FAIL(ret)) {
    } else if (OB_FAIL(get_result_spec_column_name(allocator, result_column.get_column_name_str(), name))) {
    }
  } else {
    ret = OB_INVALID_ARGUMENT;
  }
  if (OB_FAIL(ret)) {
  } else if (OB_FAIL(hidden_column.set_column_name(name))) {
  } else if (OB_FAIL(hidden_column.set_cur_default_value(default_value, false))) {
  } else if (OB_FAIL(hidden_column.set_orig_default_value(default_value))) {
  }
  return ret;
}

int ObAIColumnUtil::parse_metadata(ObIAllocator &allocator,
                                   const ObColumnSchemaV2 &column,
                                   ObAIColumnMetadata &metadata)
{
  int ret = OB_SUCCESS;
  ObIJsonBase *base = nullptr;
  ObJsonObject *object = nullptr;
  ObString function;
  if (!is_ai_column(column)) {
    ret = OB_INVALID_ARGUMENT;
  } else if (OB_FAIL(ObJsonBaseFactory::get_json_base(&allocator, column.get_comment_str(),
             ObJsonInType::JSON_TREE, ObJsonInType::JSON_TREE, base))) {
  } else if (OB_ISNULL(base) || base->json_type() != ObJsonNodeType::J_OBJECT) {
    ret = OB_INVALID_DATA;
  } else {
    object = static_cast<ObJsonObject *>(base);
    if (OB_FAIL(get_json_string(object, "function", function))
        || OB_FAIL(get_json_string(object, "model", metadata.model_name_))
        || OB_FAIL(get_json_string(object, "normalized_expr", metadata.normalized_expr_))
        || OB_FAIL(get_json_string(object, "prompt_expr", metadata.prompt_expr_))
        || OB_FAIL(get_json_string(object, "system_prompt", metadata.system_prompt_))
        || OB_FAIL(get_json_string(object, "sync_mode", metadata.sync_mode_))) {
    } else {
      metadata.function_type_ = function == "complete" ? T_FUN_SYS_AI_COMPLETE : T_FUN_SYS_AI_EMBED;
      ObJsonNode *retry_node = object->get_value("max_retries");
      ObJsonNode *dependencies = object->get_value("dependencies");
      if (OB_ISNULL(retry_node) || retry_node->json_type() != ObJsonNodeType::J_INT
          || OB_ISNULL(dependencies) || dependencies->json_type() != ObJsonNodeType::J_ARRAY) {
        ret = OB_INVALID_DATA;
      } else {
        metadata.max_retries_ = static_cast<ObJsonInt *>(retry_node)->get_int();
        ObJsonArray *array = static_cast<ObJsonArray *>(dependencies);
        for (uint64_t i = 0; OB_SUCC(ret) && i < array->element_count(); ++i) {
          ObJsonNode *id = array->get_value(i);
          if (OB_ISNULL(id) || id->json_type() != ObJsonNodeType::J_INT) {
            ret = OB_INVALID_DATA;
          } else if (OB_FAIL(metadata.dependency_column_ids_.push_back(
                         static_cast<ObJsonInt *>(id)->get_int()))) {
          }
        }
      }
    }
  }
  return ret;
}

} // namespace sql
} // namespace oceanbase
