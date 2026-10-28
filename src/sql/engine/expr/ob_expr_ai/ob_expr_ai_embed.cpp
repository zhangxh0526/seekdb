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

#define USING_LOG_PREFIX SQL_ENG
#include "ob_expr_ai_embed.h"
#include "query/ai/ob_ai_endpoint_resolver.h"
#include "share/rc/ob_server_runtime.h"

using namespace oceanbase::common;
using namespace oceanbase::sql;

namespace oceanbase 
{
namespace sql 
{
ObExprAIEmbed::ObExprAIEmbed(common::ObIAllocator &alloc)
    : ObFuncExprOperator(alloc, 
                        T_FUN_SYS_AI_EMBED, 
                        N_AI_EMBED, 
                        MORE_THAN_ZERO,
                        NOT_VALID_FOR_GENERATED_COL, 
                        NOT_ROW_DIMENSION) 
{
}

ObExprAIEmbed::~ObExprAIEmbed() 
{
}

int ObExprAIEmbed::calc_result_typeN(ObExprResType &type,
                                     ObExprResType *types_stack,
                                     int64_t param_num,
                                     common::ObExprTypeCtx &type_ctx) const 
{
  UNUSED(type_ctx);
  int ret = OB_SUCCESS;
  if (OB_UNLIKELY(param_num > 3 || param_num < 2)) {
    ObString func_name_(get_name());
    ret = OB_ERR_PARAM_SIZE;
    LOG_USER_ERROR(OB_ERR_PARAM_SIZE, func_name_.length(), func_name_.ptr());
  } else {
    types_stack[MODEL_IDX].set_calc_type(ObVarcharType);
    types_stack[MODEL_IDX].set_calc_collation_type(CS_TYPE_UTF8MB4_BIN);
    types_stack[CONTENT_IDX].set_calc_type(ObVarcharType);
    types_stack[CONTENT_IDX].set_calc_collation_type(CS_TYPE_UTF8MB4_BIN);
    if (param_num == 3) {
      if (types_stack[DIM_IDX].get_type() == ObNullType
          || ob_is_integer_type(types_stack[DIM_IDX].get_type())) {
        types_stack[DIM_IDX].set_calc_type(ObIntType);
        types_stack[DIM_IDX].set_precision(10);
        types_stack[DIM_IDX].set_scale(0);
      } else {
        ret = OB_INVALID_ARGUMENT;
        LOG_USER_ERROR(OB_INVALID_ARGUMENT, "ai_embed, dimension parameter must be an integer, not a decimal or float");
      }
    }
    type.set_varchar();
    type.set_collation_type(CS_TYPE_UTF8MB4_BIN);
    type.set_collation_level(CS_LEVEL_COERCIBLE);
  }
  return ret;
}

int ObExprAIEmbed::eval_ai_embed(const ObExpr &expr, ObEvalCtx &ctx, ObDatum &res) 
{
  INIT_SUCC(ret);
  ObDatum *arg_model_id = nullptr;
  ObDatum *arg_content = nullptr;
  ObDatum *arg_dim = nullptr;
  if (OB_FAIL(expr.eval_param_value(ctx, arg_model_id, arg_content, arg_dim))) {
  } else if (arg_model_id->is_null()) {
    ret = OB_INVALID_ARGUMENT;
    LOG_USER_ERROR(OB_INVALID_ARGUMENT, "ai_embed, model id is null");
    res.set_null();
  } else if (arg_content->is_null()) {
    res.set_null();
  } else {
    ObEvalCtx::TempAllocGuard tmp_alloc_g(ctx);
    MultimodeAlloctor temp_allocator(tmp_alloc_g.get_allocator());
    lib::ObMallocHookAttrGuard malloc_guard(lib::ObMemAttr(N_AI_EMBED));
    ObAIFuncExprInfo *info = nullptr;
    share::ObAiModelEndpointInfo resolved_endpoint;
    query::ObIAiEndpointResolver *endpoint_resolver =
        ::oceanbase::share::server_service<::oceanbase::query::ObIAiEndpointResolver>();
    ObString model_id;
    ObString content;
    if (OB_FAIL(ObTextStringHelper::read_real_string_data(
            ctx.exec_ctx_, temp_allocator, *arg_model_id, expr.args_[MODEL_IDX]->datum_meta_,
            expr.args_[MODEL_IDX]->obj_meta_.has_lob_header(), model_id))) {
    } else if (OB_FAIL(ObTextStringHelper::read_real_string_data(
                   ctx.exec_ctx_, temp_allocator, *arg_content, expr.args_[CONTENT_IDX]->datum_meta_,
                   expr.args_[CONTENT_IDX]->obj_meta_.has_lob_header(), content))) {
    } else if (model_id.empty() || content.empty()) {
      ret = OB_INVALID_ARGUMENT;
      LOG_USER_ERROR(OB_INVALID_ARGUMENT, "ai_embed, model id or input is empty");
      res.set_null();
    }
    int64_t dim = 0;
    ObJsonObject *config = nullptr;
    if (OB_FAIL(ret)) {
    } else if (OB_NOT_NULL(arg_dim) && !arg_dim->is_null()) {
      dim = arg_dim->get_int();
      ObJsonInt *dim_json = nullptr;
      if (dim <= 0) {
        ret = OB_INVALID_ARGUMENT;
        LOG_USER_ERROR(OB_INVALID_ARGUMENT, "ai_embed, dimension parameter must be a positive integer");
        res.set_null();
      } else if (OB_FAIL(ObAIFuncJsonUtils::get_json_object(temp_allocator, config))) {
      } else if (OB_FAIL(ObAIFuncJsonUtils::get_json_int(temp_allocator, dim, dim_json))) {
      } else if (OB_FAIL(config->add("dimensions", dim_json))) {
      }
    }
    if (OB_FAIL(ret)) {
    } else if (OB_FAIL(ObAIFuncUtils::get_ai_func_info(temp_allocator, model_id, info))) {
    } else if (OB_ISNULL(info) || OB_ISNULL(endpoint_resolver)) {
      ret = OB_ERR_UNEXPECTED;
    } else if (OB_FAIL(endpoint_resolver->resolve_by_model_name(
                   model_id, temp_allocator, resolved_endpoint))) {
    } else {
      ObAIFuncModel model(temp_allocator, *info, resolved_endpoint);
      ObString result;
      if (OB_FAIL(model.call_dense_embedding(content, config, result))) {
      } else if (OB_FAIL(ObAIFuncUtils::set_string_result(expr, ctx, res, result))) {
      }
    }
  }
  return ret;
}

int ObExprAIEmbed::eval_ai_embed_batch(const ObExpr &expr, ObEvalCtx &ctx,
                                       const ObBitVector &skip, const int64_t batch_size)
{
  INIT_SUCC(ret);
  ObDatumVector model_values;
  ObDatumVector content_values;
  ObDatumVector dim_values;
  ObDatum *results = expr.locate_batch_datums(ctx);
  ObBitVector &eval_flags = expr.get_evaluated_flags(ctx);
  if (batch_size == 0) {
  } else if (OB_FAIL(expr.eval_batch_param_value(
                 ctx, skip, batch_size, model_values, content_values, dim_values))) {
  } else {
    ObEvalCtx::TempAllocGuard tmp_alloc_g(ctx);
    MultimodeAlloctor temp_allocator(tmp_alloc_g.get_allocator());
    lib::ObMallocHookAttrGuard malloc_guard(lib::ObMemAttr(N_AI_EMBED));
    ObEvalCtx::BatchInfoScopeGuard batch_info_guard(ctx);
    batch_info_guard.set_batch_size(batch_size);
    ObArray<ObString> model_ids;
    ObArray<ObString> contents;
    ObArray<int64_t> dimensions;
    ObArray<bool> needs_request;
    ObArray<bool> completed;
    for (int64_t i = 0; OB_SUCC(ret) && i < batch_size; ++i) {
      if (OB_FAIL(model_ids.push_back(ObString()))
          || OB_FAIL(contents.push_back(ObString()))
          || OB_FAIL(dimensions.push_back(0))
          || OB_FAIL(needs_request.push_back(false))
          || OB_FAIL(completed.push_back(false))) {
      }
    }
    for (int64_t i = 0; OB_SUCC(ret) && i < batch_size; ++i) {
      if (skip.at(i) || eval_flags.at(i)) {
        continue;
      }
      ObDatum *model_datum = model_values.at(i);
      ObDatum *content_datum = content_values.at(i);
      if (model_datum->is_null()) {
        ret = OB_INVALID_ARGUMENT;
        LOG_USER_ERROR(OB_INVALID_ARGUMENT, "ai_embed, model id is null");
        results[i].set_null();
      } else if (content_datum->is_null()) {
        results[i].set_null();
        eval_flags.set(i);
      } else {
        int64_t dim = 0;
        if (expr.arg_cnt_ == 3 && !dim_values.at(i)->is_null()) {
          dim = dim_values.at(i)->get_int();
        }
        if (dim < 0 || (expr.arg_cnt_ == 3 && !dim_values.at(i)->is_null() && dim == 0)) {
          ret = OB_INVALID_ARGUMENT;
          LOG_USER_ERROR(OB_INVALID_ARGUMENT, "ai_embed, dimension parameter must be a positive integer");
          results[i].set_null();
        } else if (OB_FAIL(ObTextStringHelper::read_real_string_data(
                       ctx.exec_ctx_, temp_allocator, *model_datum,
                       expr.args_[MODEL_IDX]->datum_meta_,
                       expr.args_[MODEL_IDX]->obj_meta_.has_lob_header(), model_ids.at(i)))) {
        } else if (OB_FAIL(ObTextStringHelper::read_real_string_data(
                       ctx.exec_ctx_, temp_allocator, *content_datum,
                       expr.args_[CONTENT_IDX]->datum_meta_,
                       expr.args_[CONTENT_IDX]->obj_meta_.has_lob_header(), contents.at(i)))) {
        } else if (model_ids.at(i).empty() || contents.at(i).empty()) {
          ret = OB_INVALID_ARGUMENT;
          LOG_USER_ERROR(OB_INVALID_ARGUMENT, "ai_embed, model id or input is empty");
          results[i].set_null();
        } else {
          dimensions.at(i) = dim;
          needs_request.at(i) = true;
        }
      }
    }
    for (int64_t anchor = 0; OB_SUCC(ret) && anchor < batch_size; ++anchor) {
      if (!needs_request.at(anchor) || completed.at(anchor)) {
        continue;
      }
      ObArray<ObString> group_contents;
      ObArray<int64_t> group_rows;
      for (int64_t i = anchor; OB_SUCC(ret) && i < batch_size; ++i) {
        if (needs_request.at(i) && !completed.at(i)
            && dimensions.at(i) == dimensions.at(anchor)
            && model_ids.at(i) == model_ids.at(anchor)) {
          if (OB_FAIL(group_contents.push_back(contents.at(i)))
              || OB_FAIL(group_rows.push_back(i))) {
          } else {
            completed.at(i) = true;
          }
        }
      }
      ObAIFuncExprInfo *info = nullptr;
      share::ObAiModelEndpointInfo resolved_endpoint;
      query::ObIAiEndpointResolver *endpoint_resolver =
          ::oceanbase::share::server_service<::oceanbase::query::ObIAiEndpointResolver>();
      ObJsonObject *config = nullptr;
      if (OB_FAIL(ret)) {
      } else if (dimensions.at(anchor) > 0) {
        ObJsonInt *dim_json = nullptr;
        if (OB_FAIL(ObAIFuncJsonUtils::get_json_object(temp_allocator, config))
            || OB_FAIL(ObAIFuncJsonUtils::get_json_int(temp_allocator, dimensions.at(anchor), dim_json))
            || OB_FAIL(config->add("dimensions", dim_json))) {
        }
      }
      ObArray<ObString> embedding_results;
      if (OB_FAIL(ret)) {
      } else if (OB_FAIL(ObAIFuncUtils::get_ai_func_info(temp_allocator, model_ids.at(anchor), info))) {
      } else if (OB_ISNULL(info) || OB_ISNULL(endpoint_resolver)) {
        ret = OB_ERR_UNEXPECTED;
      } else if (OB_FAIL(endpoint_resolver->resolve_by_model_name(
                     model_ids.at(anchor), temp_allocator, resolved_endpoint))) {
      } else {
        ObAIFuncModel model(temp_allocator, *info, resolved_endpoint);
        if (OB_FAIL(model.call_dense_embedding_vector_v2(group_contents, config, embedding_results))) {
        } else if (embedding_results.count() != group_rows.count()) {
          ret = OB_ERR_UNEXPECTED;
        }
      }
      for (int64_t i = 0; OB_SUCC(ret) && i < group_rows.count(); ++i) {
        const int64_t row_idx = group_rows.at(i);
        batch_info_guard.set_batch_idx(row_idx);
        if (OB_FAIL(ObAIFuncUtils::set_string_result(
                expr, ctx, results[row_idx], embedding_results.at(i)))) {
        } else {
          eval_flags.set(row_idx);
        }
      }
    }
  }
  return ret;
}

int ObExprAIEmbed::cg_expr(ObExprCGCtx &expr_cg_ctx, 
                           const ObRawExpr &raw_expr,
                           ObExpr &rt_expr) const 
{
  int ret = OB_SUCCESS;
  // TODO: support schema version match in plan cache for ai func
  // const ObRawExpr *model_key = raw_expr.get_param_expr(0);
  // if (OB_NOT_NULL(model_key)
  //     && (model_key->is_static_scalar_const_expr() || model_key->is_const_expr())
  //     && model_key->get_expr_type() != T_OP_GET_USER_VAR &&
  //     OB_NOT_NULL(expr_cg_ctx.schema_guard_)) {
  //   ObIAllocator *allocator = expr_cg_ctx.allocator_;
  //   ObExecContext *exec_ctx = expr_cg_ctx.session_->get_cur_exec_ctx();
  //   ObObj const_data;
  //   bool got_data = false;
  //   ObAIFuncExprInfo *info = nullptr;
  //   if (OB_ISNULL(allocator)) {
  //     ret = OB_ERR_UNEXPECTED;
  //     LOG_WARN("allocator is null", K(ret));
  //   } else if (OB_FAIL(ObSQLUtils::calc_const_or_calculable_expr(exec_ctx,
  //                                                         model_key,
  //                                                         const_data,
  //                                                         got_data,
  //                                                         *allocator))) {
  //     LOG_WARN("failed to calc offset expr", K(ret));
  //   } else if (!got_data || const_data.is_null()) {
  //   } else if (OB_FAIL(ObAIFuncUtils::get_ai_func_info(*allocator, const_data.get_string(), *expr_cg_ctx.schema_guard_, info))) {
  //     LOG_WARN("failed to get ai func info", K(ret), K(const_data.get_string()));
  //   } else {
  //     rt_expr.extra_info_ = info;
  //   }
  // }

  if (OB_SUCC(ret)) {
    rt_expr.eval_func_ = ObExprAIEmbed::eval_ai_embed;
    rt_expr.eval_batch_func_ = ObExprAIEmbed::eval_ai_embed_batch;
  }
  return ret;
}

} // namespace sql
} // namespace oceanbase
