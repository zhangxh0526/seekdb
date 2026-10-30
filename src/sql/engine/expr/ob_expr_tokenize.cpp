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

#include "sql/engine/expr/ob_expr_tokenize.h"

#include "lib/alloc/alloc_struct.h"
#include "lib/allocator/page_arena.h"
#include "lib/charset/ob_charset.h"
#include "common/json_type/ob_json_base.h"
#include "common/json_type/ob_json_tree.h"
#include "common/json_type/ob_json_bin.h"
#include "lib/ob_errno.h"
#include "lib/oblog/ob_log_module.h"
#include "lib/string/ob_string.h"
#include "lib/utility/ob_macro_utils.h"
#include "object/ob_object.h"
#include "sql/resolver/ddl/ob_fts_index_builder_util.h"
#include "share/ob_json_access_utils.h"
#include "data_plane/fts/ob_fts_parser_helper.h"
#include "data_plane/fts/dict/ob_gen_dic_loader.h"
#include "data_plane/fts/ob_fts_parser_property.h"

#define USING_LOG_PREFIX SQL_ENG
#include "sql/engine/expr/ob_expr_json_func_helper.h" // file not self-contained, there're logs inside.

namespace oceanbase
{
namespace sql
{
namespace
{
// Without a properties argument, the parser and its properties only depend on the parser name argument,
// and a loaded dictionary stays loaded. Every thread remembers its last resolution so that repeated calls
// skip building, serializing and parsing back the properties.
class ObTokenizeParserCache final
{
public:
  bool get(const ObString &raw_parser_name,
           const ObCharsetType charset,
           storage::ObFTParser &parser,
           storage::ObFTParserProperty &property) const
  {
    bool hit = false;
    if (valid_ && charset == charset_
        && 0 == raw_parser_name.compare(ObString(raw_name_len_, raw_name_))) {
      // copied out: the values must not change if a nested call refills the cache
      parser = parser_;
      property = property_;
      hit = true;
    }
    return hit;
  }
  void put(const ObString &raw_parser_name,
           const ObCharsetType charset,
           const storage::ObFTParser &parser,
           const storage::ObFTParserProperty &property)
  {
    valid_ = raw_parser_name.length() <= MAX_NAME_LEN;
    if (valid_) {
      charset_ = charset;
      raw_name_len_ = raw_parser_name.length();
      MEMCPY(raw_name_, raw_parser_name.ptr(), raw_name_len_);
      parser_ = parser;
      property_ = property;
    }
  }
private:
  static const int64_t MAX_NAME_LEN = storage::OB_FT_PARSER_NAME_LENGTH;
  bool valid_ = false;
  ObCharsetType charset_ = CHARSET_INVALID;
  int32_t raw_name_len_ = 0;
  char raw_name_[MAX_NAME_LEN];
  // the property only refers to static strings, so it can outlive the call that parsed it
  storage::ObFTParser parser_;
  storage::ObFTParserProperty property_;
};

thread_local ObTokenizeParserCache tokenize_parser_cache;

// The JSON binary of the token array, byte for byte what ObJsonBinSerializer makes of an ObJsonArray
// holding one ObJsonString per token, without building that tree and an ObJsonBin over it.
int serialize_token_array(const storage::ObFTWordMap &words, ObIAllocator &allocator, ObString &json_bin)
{
  int ret = OB_SUCCESS;
  const ObJBVerType string_vertype = ObJsonVerType::get_json_vertype(ObJsonNodeType::J_STRING);
  const uint64_t count = words.size();
  // the serialize size estimation of ObJsonArray::update_serialize_size
  uint64_t obj_size = 0;
  {
    const uint64_t ESTIMATE_ARRAY_SIZE = sizeof(uint32_t);
    const uint64_t ESTIMATE_OFFSET_SIZE = sizeof(uint32_t);
    const uint64_t TYPE_SIZE = sizeof(uint8_t);
    uint64_t node_total_size = 0;
    uint64_t node_size = 0;
    for (storage::ObFTWordMap::const_iterator it = words.begin(); it != words.end(); ++it) {
      const uint64_t len = it->first.get_word().get_string().length();
      node_size = sizeof(uint8_t) + serialization::encoded_length_vi64(len) + len;
      node_total_size += node_size;
    }
    const uint64_t count_size = ObJsonVar::get_var_size(ObJsonVar::get_var_type(count));
    const uint64_t estimated_total_size = OB_JSON_BIN_OBJ_HEADER_LEN + count_size + ESTIMATE_ARRAY_SIZE
        + (ESTIMATE_OFFSET_SIZE + TYPE_SIZE) * count + node_total_size;
    const uint64_t offset_type_size =
        ObJsonVar::get_var_size(ObJsonVar::get_var_type(estimated_total_size - node_size));
    const uint64_t total_offset_size = (offset_type_size + TYPE_SIZE) * count;
    const uint64_t total_size = OB_JSON_BIN_OBJ_HEADER_LEN + count_size + ESTIMATE_ARRAY_SIZE
        + total_offset_size + node_total_size;
    const uint64_t array_size = ObJsonVar::get_var_size(ObJsonVar::get_var_type(total_size));
    obj_size = OB_JSON_BIN_OBJ_HEADER_LEN + count_size + array_size + total_offset_size + node_total_size;
  }
  ObJsonBuffer result(&allocator);
  int64_t start_pos = 0;
  if (OB_FAIL(ObJsonBin::add_doc_header_v0(result))) {
    LOG_WARN("failed to add doc header", K(ret));
  } else {
    start_pos = result.length();
  }
  // ObJsonBinSerializer::serialize_json_object: serialize again with the real size when it needs a
  // wider size type than the estimation
  bool done = false;
  for (uint32_t depth = 0; OB_SUCC(ret) && !done; ++depth) {
    ObJsonBinMeta meta;
    meta.set_type(ObJsonBin::get_array_vertype(), false);
    meta.set_element_count(count);
    meta.set_element_count_var_type(ObJsonVar::get_var_type(count));
    meta.set_obj_size(obj_size);
    meta.set_obj_size_var_type(ObJsonVar::get_var_type(obj_size));
    meta.set_entry_var_type(meta.obj_size_var_type());
    meta.set_is_continuous(true);
    meta.calc_entry_array();
    if (OB_FAIL(result.reserve(obj_size))) {
      LOG_WARN("failed to reserve", K(ret), K(obj_size));
    } else if (OB_FAIL(meta.to_header(result))) {
      LOG_WARN("failed to write header", K(ret));
    }
    int idx = 0;
    for (storage::ObFTWordMap::const_iterator it = words.begin();
         OB_SUCC(ret) && it != words.end(); ++it, ++idx) {
      // result may realloc, so locate the entries again for every value
      if (OB_FAIL(ObJsonBinSerializer::set_value_entry(meta, result.ptr() + start_pos, idx,
                                                       result.length() - start_pos, string_vertype,
                                                       false /*check*/))) {
        LOG_WARN("failed to set value entry", K(ret), K(idx));
      } else if (OB_FAIL(ObJsonBinSerializer::serialize_json_string(
                     string_vertype, it->first.get_word().get_string(), result))) {
        LOG_WARN("failed to serialize token", K(ret), K(idx));
      }
    }
    if (OB_SUCC(ret)) {
      const uint64_t real_obj_size = static_cast<uint64_t>(result.length() - start_pos);
      if (ObJsonVar::get_var_type(real_obj_size) <= ObJsonVar::get_var_type(obj_size)) {
        if (OB_FAIL(ObJsonBinSerializer::set_obj_size(meta, result.ptr() + start_pos, real_obj_size))) {
          LOG_WARN("failed to set obj size", K(ret));
        } else {
          done = true;
        }
      } else if (depth >= OB_JSON_BIN_MAX_SERIALIZE_TIME) {
        ret = OB_ERR_UNEXPECTED;
        LOG_WARN("too many serialize times", K(ret), K(depth), K(obj_size), K(real_obj_size));
      } else {
        obj_size = real_obj_size;
        result.set_length(start_pos);
      }
    }
  }
  if (OB_FAIL(ret)) {
  } else if (OB_FAIL(ObJsonBin::set_doc_header_v0(result, result.length(),
                                                  false /*use_lexicographical_order*/))) {
    LOG_WARN("failed to set doc header", K(ret));
  } else {
    result.get_result_string(json_bin);
  }
  return ret;
}

int eval_raw_parser_name(const ObExpr &expr, ObEvalCtx &ctx, ObString &raw_parser_name)
{
  int ret = OB_SUCCESS;
  ObDatum *parser_datum = nullptr;
  if (expr.arg_cnt_ < 2) {
    raw_parser_name = ObString::make_string(OB_DEFAULT_FULLTEXT_PARSER_NAME);
  } else if (OB_FAIL(expr.args_[1]->eval(ctx, parser_datum))) {
  } else {
    if (parser_datum->is_null()) {
      raw_parser_name = ObString::make_string(OB_DEFAULT_FULLTEXT_PARSER_NAME);
    } else {
      ObString name = parser_datum->get_string();
      raw_parser_name = name.trim();
    }
  }
  return ret;
}
} // namespace

ObExprTokenize::ObExprTokenize(common::ObIAllocator &alloc)
    : ObStringExprOperator(alloc,
                           T_FUN_TOKENIZE,
                           N_TOKENIZE,
                           MORE_THAN_ZERO,
                           VALID_FOR_GENERATED_COL)
{
}

ObExprTokenize::~ObExprTokenize() {}

int ObExprTokenize::eval_tokenize(const ObExpr &expr, ObEvalCtx &ctx, ObDatum &expr_datum)
{
  int ret = OB_SUCCESS;

  ObEvalCtx::TempAllocGuard tmp_alloc_g(ctx);
  common::ObArenaAllocator &temp_allocator = tmp_alloc_g.get_allocator();

  ObString json_result;
  TokenizeParam param;

  // check param num, which is checked in ObExprOperator::calc_result_typeN.
  if (OB_UNLIKELY(expr.arg_cnt_ < 1 || expr.arg_cnt_ > 3)) {
    ret = OB_INVALID_ARGUMENT;
  } else if (OB_FAIL(parse_param(expr, ctx, temp_allocator, param))) {
  } else if (OB_FAIL(tokenize_fulltext(param, param.output_mode_, temp_allocator, json_result))) {
  } else if (OB_FAIL(ObJsonExprHelper::pack_json_str_res(expr, ctx, expr_datum, json_result))) {
  }

  return ret;
}

int ObExprTokenize::tokenize_fulltext(const TokenizeParam &param,
                                      TokenizeParam::OUTPUT_MODE mode,
                                      ObIAllocator &allocator,
                                      ObString &result)
{
  int ret = OB_SUCCESS;
  storage::ObFTParseHelper tokenize_helper;
  const int64_t ft_word_bkt_cnt = MIN(MAX(param.fulltext_.length() / 2, 2), 997);
  int64_t doc_len = 0;
  storage::ObFTWordMap token_map;

  ObArenaAllocator tmp_parse_alloc(ObMemAttr("Tmp buffer"));

  if (TokenizeParam::OUTPUT_MODE::DEFAULT != mode && TokenizeParam::OUTPUT_MODE::ALL != mode) {
    ret = OB_INVALID_ARGUMENT;
  } else if (param.is_parser_parsed_
             && OB_FAIL(tokenize_helper.init(&allocator, param.parser_, param.parser_property_))) {
  } else if (!param.is_parser_parsed_
             && OB_FAIL(tokenize_helper.init(&allocator, param.parser_name_, param.properties_))) {
  } else if (OB_FAIL(token_map.create(ft_word_bkt_cnt, common::ObMemAttr("FTWordMap")))) {
  } else if (
      (0 != param.fulltext_.length())
      && OB_FAIL(tokenize_helper.segment(
                     param.meta_,
                     param.fulltext_.ptr(),
                     param.fulltext_.length(),
                     doc_len,
                     token_map))) {
  } else {
    switch (param.output_mode_) {
    case TokenizeParam::OUTPUT_MODE::DEFAULT: {
      if (OB_FAIL(serialize_token_array(token_map, allocator, result))) {
      }
      break;
    }
    case TokenizeParam::OUTPUT_MODE::ALL: {
      ObIJsonBase *detail = nullptr;
      if (OB_FAIL(tokenize_helper.make_detail_json(token_map, doc_len, detail))) {
      } else if (OB_FAIL(share::ObJsonWrapper::get_raw_binary(detail, result, &allocator))) {
      }
      break;
    }
    default:
      ret = OB_INVALID_ARGUMENT;
    }
  }
  return ret;
}

ObExprTokenize::TokenizeParam ::TokenizeParam()
  : allocator_(ObMemAttr("TokenizeParam")),
    parser_name_(ObString(OB_DEFAULT_FULLTEXT_PARSER_NAME)),
    meta_(),
    fulltext_(),
    output_mode_(OUTPUT_MODE::DEFAULT),
    is_parser_parsed_(false),
    parser_(),
    parser_property_()
{
}

int ObExprTokenize::TokenizeParam::parse_json_param(const ObIJsonBase *obj)
{
  int ret = OB_SUCCESS;
  ObString str;
  ObIJsonBase *val;

  if (OB_UNLIKELY(nullptr == obj)) {
    ret = OB_INVALID_ARGUMENT;
  } else if (ObJsonNodeType::J_OBJECT != obj->json_type()) {
    ret = OB_INVALID_ARGUMENT;
  } else if (obj->element_count() == 0) {
    // no data
  } else if (OB_FAIL(obj->get_object_value(0, str, val))) {
  } else if (0 == str.case_compare(CASE_INDICATOR_STR)) {
    if (ObJsonNodeType::J_STRING != val->json_type()) {
      ret = OB_INVALID_ARGUMENT;
    } else if (0 == ObString(val->get_data_length(), val->get_data()).case_compare("UPPER")) {
    } else if (0 == ObString(val->get_data_length(), val->get_data()).case_compare("LOWER")) {
    } else {
      ret = OB_INVALID_ARGUMENT;
    }
    ret = OB_NOT_SUPPORTED;
    LOG_USER_ERROR(OB_NOT_SUPPORTED, "case indentifier");
  } else if (0 == str.case_compare(OUTPUT_MODE_STR)) {
    if (ObJsonNodeType::J_STRING != val->json_type()) {
      ret = OB_INVALID_ARGUMENT;
      LOG_USER_ERROR(OB_INVALID_ARGUMENT, "output mode should be string default or all");
    } else if (0 == ObString(val->get_data_length(), val->get_data()).case_compare("DEFAULT")) {
      output_mode_ = DEFAULT;
    } else if (0 == ObString(val->get_data_length(), val->get_data()).case_compare("ALL")) {
      output_mode_ = ALL;
    } else {
      ret = OB_INVALID_ARGUMENT;
      LOG_USER_ERROR(OB_INVALID_ARGUMENT, "output mode should be string default or all");
    }
  } else if (0 == str.case_compare(STOPWORDS_LIST_STR)) {
    ret = OB_NOT_SUPPORTED;
    LOG_USER_ERROR(OB_NOT_SUPPORTED, "stopwords");
  } else if (0 == str.case_compare(ADDITIONAL_ARGS_STR)) {
    if (ObJsonNodeType::J_ARRAY != val->json_type()) {
      ret = OB_INVALID_ARGUMENT;
      LOG_USER_ERROR(OB_INVALID_ARGUMENT, "parser arguments");
    } else {
      ObString json_str;
      if (OB_FAIL(storage::ObFTParserJsonProps::tokenize_array_to_props_json(allocator_, val, json_str))) {
        ObSqlString message;
        message.append_fmt("format in %s form", ADDITIONAL_ARGS_STR);
        LOG_USER_ERROR(OB_INVALID_ARGUMENT, message.ptr());
      } else {
        properties_ = json_str;
      }
    }
  } else {
    ret = OB_INVALID_ARGUMENT;
    ObSqlString message;
    message.append_fmt("config: %s", str.ptr());
    LOG_USER_ERROR(OB_INVALID_ARGUMENT, message.ptr());
  }
  return ret;
}

int ObExprTokenize::parse_param(const ObExpr &expr,
                                ObEvalCtx &ctx,
                                common::ObArenaAllocator &allocator,
                                TokenizeParam &param)
{
  int ret = OB_SUCCESS;

  ObDatum *parser_params_datum;
  ObString raw_parser_name = ObString::make_string(OB_DEFAULT_FULLTEXT_PARSER_NAME);

  ObEvalCtx::TempAllocGuard tmp_alloc_g(ctx);
  
  MultimodeAlloctor temp_allocator(tmp_alloc_g.get_allocator());

  if (OB_UNLIKELY(expr.arg_cnt_ < 1 || expr.arg_cnt_ > 3)) {
    ret = OB_INVALID_ARGUMENT;
  } else if (OB_FAIL(parse_fulltext(expr, ctx, param))) {
  } else if (expr.arg_cnt_ < 3) {
    const ObCharsetType charset = ObCharset::charset_type_by_coll(param.meta_.get_collation_type());
    ObString raw_parser_name;
    if (OB_FAIL(eval_raw_parser_name(expr, ctx, raw_parser_name))) {
    } else if (tokenize_parser_cache.get(raw_parser_name, charset, param.parser_, param.parser_property_)) {
      param.is_parser_parsed_ = true;
    } else if (OB_FAIL(construct_ft_parser_inner_name(raw_parser_name, param))) {
    } else if (OB_FAIL(param.reform_parser_properties(param.properties_))) {
    } else if (OB_FAIL(param.try_load_dictionary_for_ik())) {
    } else if (OB_FAIL(param.parser_.parse_from_str(param.parser_name_.ptr(), param.parser_name_.length()))) {
    } else if (OB_FAIL(param.parser_property_.parse_for_parser_helper(param.parser_, param.properties_))) {
    } else {
      param.is_parser_parsed_ = true;
      tokenize_parser_cache.put(raw_parser_name, charset, param.parser_, param.parser_property_);
    }
  } else if (OB_FAIL(parse_parser_name(expr, ctx, param))) {
  } else if (OB_FAIL(parse_parser_properties(expr, ctx, temp_allocator, param))) {
  } else if (OB_FAIL(param.reform_parser_properties(param.properties_))) {
  } else if (OB_FAIL(param.try_load_dictionary_for_ik())) {
  }
  return ret;
}

int ObExprTokenize::construct_ft_parser_inner_name(const ObString &input_str, TokenizeParam &param)
{
  int ret = OB_SUCCESS;
  storage::ObFTParser parser;

  char *parser_name_buf = nullptr;
  if (OB_ISNULL(parser_name_buf
                = static_cast<char *>(param.allocator_.alloc(storage::OB_FT_PARSER_NAME_LENGTH)))) {
    ret = OB_ALLOCATE_MEMORY_FAILED;
  } else if (OB_FAIL(parser.init(input_str))) {
  } else if (OB_FAIL(parser.serialize_to_str(parser_name_buf, storage::OB_FT_PARSER_NAME_LENGTH))) {
  } else {
    param.parser_name_ = ObString::make_string(parser_name_buf);
  }
  return ret;
}

int ObExprTokenize::calc_result_typeN(ObExprResType &type,
                                      ObExprResType *types,
                                      int64_t param_num,
                                      common::ObExprTypeCtx &type_ctx) const
{
  int ret = OB_SUCCESS;

  if (OB_UNLIKELY(param_num < 1 || param_num > 3)) {
    ret = OB_ERR_PARAM_SIZE;
    ObString expr_name(N_TOKENIZE);
    LOG_USER_ERROR(OB_ERR_PARAM_SIZE, expr_name.length(), expr_name.ptr());
  } else {
    // just okay
  }

  ObLength length = ObAccuracy::DDL_DEFAULT_ACCURACY[ObJsonType].get_length();

  if (OB_SUCC(ret)) {
    // set res type
    type.set_json();
    type.set_length(length); // keep consistent with other json expr, maybe calc it later.

    // param type set, skip charset after first param
    for (int64_t i = 1; OB_SUCC(ret) && i < param_num; ++i) {
      if (ob_is_string_type(types[i].get_type())) {
        if (types[i].get_charset_type() != CHARSET_UTF8MB4) {
          types[i].set_calc_collation_type(CS_TYPE_UTF8MB4_BIN);
        }
      }
    }

    // handle param
    if (param_num >= 2) {
      types[1].set_varchar();
    }

    if (param_num >= 3) {
      if (OB_FAIL(ObJsonExprHelper::is_valid_for_json(types, 2, N_TOKENIZE))) {
      }
    }
  }

  return ret;
}

int ObExprTokenize::cg_expr(ObExprCGCtx &op_cg_ctx,
                            const ObRawExpr &raw_expr,
                            ObExpr &rt_expr) const
{
  int ret = OB_SUCCESS;
  CK((rt_expr.arg_cnt_ >= 1 && rt_expr.arg_cnt_ <= 3));
  if (OB_SUCC(ret)) {
    // do register
    rt_expr.eval_func_ = eval_tokenize;
  }
  return ret;
}

int ObExprTokenize::parse_fulltext(const ObExpr &expr, ObEvalCtx &ctx, TokenizeParam &param)
{
  int ret = OB_SUCCESS;

  ObDatum *fulltext_datum;

  if (OB_FAIL(expr.args_[0]->eval(ctx, fulltext_datum))) {
  } else {
    if (fulltext_datum->is_null()) {
      // do nothing, return empty result
      param.fulltext_ = ObString::make_empty_string();
    } else {
      param.fulltext_ = fulltext_datum->get_string();
    }
    param.meta_.set_varchar(); // as we hardcoded in fts_index
    param.meta_.set_collation_type(expr.args_[0]->obj_meta_.get_collation_type());
  }
  return ret;
}

int ObExprTokenize::parse_parser_name(const ObExpr &expr, ObEvalCtx &ctx, TokenizeParam &param)
{
  int ret = OB_SUCCESS;
  ObString raw_parser_name;

  if (OB_FAIL(eval_raw_parser_name(expr, ctx, raw_parser_name))) {
    // already logged
  } else if (OB_FAIL(construct_ft_parser_inner_name(raw_parser_name, param))) {
  }

  return ret;
}

int ObExprTokenize::parse_parser_properties(const ObExpr &expr,
                                            ObEvalCtx &ctx,
                                            MultimodeAlloctor &mm_alloc,
                                            TokenizeParam &param)
{
  int ret = OB_SUCCESS;
  ObIJsonBase *base = nullptr;

  if (expr.arg_cnt_ < 3) {
    // do nothing
  } else {
    bool is_null = false;
    if (OB_FAIL(ObJsonExprHelper::get_json_doc(expr, ctx, mm_alloc, 2, base, is_null))) {
    } else {
      if (ObJsonNodeType::J_ARRAY != base->json_type()) {
        ret = OB_INVALID_ARGUMENT;
        LOG_USER_ERROR(OB_INVALID_ARGUMENT, "parser args should be in an array.");
      } else {
        for (uint64_t i = 0; OB_SUCC(ret) && i < base->element_count(); ++i) {
          ObIJsonBase *node = nullptr;
          if (OB_FAIL(base->get_array_element(i, node))) {
          } else if (ObJsonNodeType::J_OBJECT != (node->json_type())) {
            ret = OB_INVALID_ARGUMENT;
          } else if (OB_FAIL(param.parse_json_param(node))) {
          }
        } // for
      }
    }
  }

  return ret;
}

int ObExprTokenize::TokenizeParam::reform_parser_properties(const ObString &properties)
{
  int ret = OB_SUCCESS;
  storage::ObFTParserJsonProps parser_properties;

  if (OB_FAIL(parser_properties.init())) {
  } else if (OB_FAIL(parser_properties.parse_from_valid_str(properties))) {
    LOG_USER_ERROR(OB_INVALID_ARGUMENT, "parser properties invalid.");
  } else if (OB_FAIL(parser_properties.rebuild_props_for_ddl(parser_name_,
                                                             ObCollationType::CS_TYPE_UTF8MB4_BIN,
                                                             true))) {
  } else if (OB_FAIL(parser_properties.to_format_json(allocator_, properties_))) {
  }

  return ret;
}

int ObExprTokenize::TokenizeParam::try_load_dictionary_for_ik()
{
  int ret = OB_SUCCESS;
  bool need_to_load_dic = false;
  storage::ObDicLoaderHandle dic_loader_handle;
  if (OB_FAIL(share::ObFtsIndexBuilderUtil::check_need_to_load_dic(
          parser_name_, need_to_load_dic))) {
  } else if (need_to_load_dic) {
    if (OB_FAIL(storage::ObGenDicLoader::get_instance().get_dic_loader(
                    ObString::make_string(storage::ObFTSLiteral::PARSER_NAME_IK), // currently only ik, use parser_name_ without version suffix
                    ObCharset::charset_type_by_coll(meta_.get_collation_type()),
                    dic_loader_handle))) {
    } else if (OB_UNLIKELY(!dic_loader_handle.is_valid())) {
      ret = OB_ERR_UNEXPECTED;
    } else if (OB_FAIL(dic_loader_handle.get_loader()->try_load_dictionary_in_trans())) {
    }
  }
  return ret;
}

} // namespace sql
} // namespace oceanbase
