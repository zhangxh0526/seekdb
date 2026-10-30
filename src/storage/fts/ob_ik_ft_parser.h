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

#ifndef _OCEANBASE_STORAGE_FTS_OB_IK_FT_PARSER_H_
#define _OCEANBASE_STORAGE_FTS_OB_IK_FT_PARSER_H_

#include "lib/allocator/ob_allocator.h"
#include "lib/allocator/page_arena.h"
#include "storage/fts/dict/ob_ft_cache_container.h"
#include "storage/fts/dict/ob_ft_dict.h"
#include "storage/fts/dict/ob_ft_dict_def.h"
#include "storage/fts/ik/ob_ik_processor.h"
#include "data_plane/fts/ob_fts_parser.h"

#include <cstdint>
#include <mutex>
namespace oceanbase
{
namespace storage
{
class ObFTDictHub;
class ObIKLetterProcessor;
class ObIKQuantifierProcessor;
class ObIKCJKProcessor;
class ObIKSurrogateProcessor;

class ObIKFTParser final : public ObITokenIterator
{
public:
  ObIKFTParser(ObIAllocator &allocator, ObIFTDict *dict_main, ObIFTDict *dict_quan)
      : allocator_(allocator),
        is_inited_(false),
        coll_type_(ObCollationType::CS_TYPE_INVALID),
        ctx_(nullptr),
        letter_segmenter_(nullptr),
        quantifier_segmenter_(nullptr),
        cjk_segmenter_(nullptr),
        surrogate_segmenter_(nullptr),
        segmenters_(allocator_),
        dict_main_(dict_main),
        dict_quan_(dict_quan)
  {
  }

  virtual ~ObIKFTParser() { reset(); }

  int init(const ObFTParserParam &param);

  int get_next_token(const char *&word,
                     int64_t &word_len,
                     int64_t &char_cnt,
                     int64_t &word_freq) override;

  VIRTUAL_TO_STRING_KV(K(is_inited_));

private:
  int produce();

  int process_next_batch();

  int process_one_char(TokenizeContext &ctx,
                       const char *ch,
                       const uint8_t char_len,
                       const ObFTCharUtil::CharType type);

private:
  int init_segmenter(const ObFTParserParam &param);

  int init_ctx(const ObFTParserParam &param);

  void reset();

private:
  static constexpr int SEGMENT_LIMIT = 1000;
  ObIAllocator &allocator_;
  bool is_inited_;

  ObCollationType coll_type_;
  TokenizeContext *ctx_;
  ObIKLetterProcessor *letter_segmenter_;
  ObIKQuantifierProcessor *quantifier_segmenter_;
  ObIKCJKProcessor *cjk_segmenter_;
  ObIKSurrogateProcessor *surrogate_segmenter_;
  ObList<ObIIKProcessor *, ObIAllocator> segmenters_;

  ObIFTDict *dict_main_;
  ObIFTDict *dict_quan_;

  DISABLE_COPY_ASSIGN(ObIKFTParser);
};

class ObIKFTParserDesc final : public ObIFTParserDesc
{
public:
  ObIKFTParserDesc()
      : allocator_(lib::ObMemAttr("IKSharedDict")),
        cache_main_(allocator_),
        cache_quan_(allocator_),
        dict_inited_(false),
        dict_main_(nullptr),
        dict_quan_(nullptr)
  {
  }
  virtual ~ObIKFTParserDesc() override;
  virtual int segment(ObFTParserParam *param, ObITokenIterator *&iter) const override;
  virtual void free_token_iter(ObFTParserParam *param, ObITokenIterator *&iter) const override;
  virtual int get_add_word_flag(ObAddWordFlag &flag) const override;

private:
  int init_shared_dicts(ObFTDictHub *hub) const;
  int load_shared_dict(ObFTDictHub *hub,
                       const ObFTDictDesc &desc,
                       ObFTCacheRangeContainer &container,
                       ObIFTDict *&dict) const;
  void destroy_shared_dicts() const;

private:
  mutable std::mutex dict_mutex_;
  mutable ObArenaAllocator allocator_;
  mutable ObFTCacheRangeContainer cache_main_;
  mutable ObFTCacheRangeContainer cache_quan_;
  mutable bool dict_inited_;
  mutable ObIFTDict *dict_main_;
  mutable ObIFTDict *dict_quan_;
};

} //  namespace storage
} //  namespace oceanbase

#endif // _OCEANBASE_STORAGE_FTS_OB_IK_FT_PARSER_H_
