// Copyright 2018 The Chromium Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_MEDIA_CODEC_CODECS_VAAPI_THIRD_PARTY_CHROMIUM_H264_VAAPI_VIDEO_ENCODER_DELEGATE_H_
#define SRC_MEDIA_CODEC_CODECS_VAAPI_THIRD_PARTY_CHROMIUM_H264_VAAPI_VIDEO_ENCODER_DELEGATE_H_

#include <stddef.h>

// Fuchsia change.
// #include "base/containers/circular_deque.h"
#include "src/media/third_party/chromium_media/media/filters/h264_bitstream_buffer.h"
#include "src/media/third_party/chromium_media/media/gpu/h264_dpb.h"
#include "vaapi_video_encoder_delegate.h"

namespace media {
class VaapiWrapper;

// This class provides an H264 encoder functionality, generating stream headers,
// managing encoder state, reference frames, and other codec parameters, while
// requiring support from an Accelerator to encode frame data based on these
// parameters.
//
// This class must be created, called and destroyed on a single sequence.
//
// Names used in documentation of this class refer directly to naming used
// in the H.264 specification (http://www.itu.int/rec/T-REC-H.264).
class H264VaapiVideoEncoderDelegate : public VaapiVideoEncoderDelegate {
 public:
  struct EncodeParams {
    EncodeParams();

    VideoBitrateAllocation bitrate_allocation;

    // Framerate in FPS.
    uint32_t framerate;

    // Bitrate window size in ms.
    uint32_t cpb_window_size_ms;

    // Bitrate window size in bits.
    unsigned int cpb_size_bits;

    // Quantization parameter. Their ranges are 0-51.
    uint8_t initial_qp;
    uint8_t min_qp;
    uint8_t max_qp;

    // Maxium Number of Reference frames.
    size_t max_num_ref_frames;

    // Maximum size of reference picture list 0.
    size_t max_ref_pic_list0_size;

    uint32_t gop_length{};
  };

  H264VaapiVideoEncoderDelegate(scoped_refptr<VaapiWrapper> vaapi_wrapper,
                                base::RepeatingClosure error_cb);

  H264VaapiVideoEncoderDelegate(const H264VaapiVideoEncoderDelegate&) = delete;
  H264VaapiVideoEncoderDelegate& operator=(const H264VaapiVideoEncoderDelegate&) = delete;

  ~H264VaapiVideoEncoderDelegate() override;

  // VaapiVideoEncoderDelegate implementation.
  bool Initialize(const VideoEncodeAccelerator::Config& config,
                  const VaapiVideoEncoderDelegate::Config& ave_config) override;
  bool UpdateRates(const VideoBitrateAllocation& bitrate_allocation, uint32_t framerate) override;
  gfx::Size GetCodedSize() const override;
  size_t GetMaxNumOfRefFrames() const override;
  std::vector<gfx::Size> GetSVCLayerResolutions() override;

 private:
  class TemporalLayers;

  friend class H264VaapiVideoEncoderDelegateTest;

  bool PrepareEncodeJob(EncodeJob& encode_job) override;
  BitstreamBufferMetadata GetMetadata(const EncodeJob& encode_job, size_t payload_size) override;

  // Fill current_sps_ and current_pps_ with current encoding state parameters.
  void UpdateSPS();
  void UpdatePPS();

  // Generate packed SPS and PPS in packed_sps_ and packed_pps_, using values
  // in current_sps_ and current_pps_.
  void GeneratePackedSPS();
  void GeneratePackedPPS();

  // Generate packed slice header from |pic_param|, |slice_param| and |pic|.
  scoped_refptr<H264BitstreamBuffer> GeneratePackedSliceHeader(
      const VAEncPictureParameterBufferH264& pic_param,
      const VAEncSliceParameterBufferH264& sliice_param, const H264Picture& pic);

  // Check if |bitrate| and |framerate| and current coded size are supported by
  // current profile and level.
  bool CheckConfigValidity(uint32_t bitrate, uint32_t framerate);

  // Submits a H264BitstreamBuffer |buffer| to the driver.
  bool SubmitH264BitstreamBuffer(const H264BitstreamBuffer& buffer);
  // Submits a VAEncMiscParameterBuffer |data| whose size and type are |size|
  // and |type| to the driver.
  bool SubmitVAEncMiscParamBuffer(VAEncMiscParameterType type, const void* data, size_t size);

  bool SubmitPackedHeaders(scoped_refptr<H264BitstreamBuffer> packed_sps,
                           scoped_refptr<H264BitstreamBuffer> packed_pps);

  bool SubmitFrameParameters(EncodeJob& job,
                             const H264VaapiVideoEncoderDelegate::EncodeParams& encode_params,
                             const H264SPS& sps, const H264PPS& pps, scoped_refptr<H264Picture> pic,
                             const base::circular_deque<scoped_refptr<H264Picture>>& ref_pic_list0,
                             const std::optional<size_t>& ref_frame_index);

  // Current SPS, PPS and their packed versions. Packed versions are NALUs
  // in AnnexB format *without* emulation prevention three-byte sequences
  // (those are expected to be added by the client as needed).
  H264SPS current_sps_;
  scoped_refptr<H264BitstreamBuffer> packed_sps_;
  H264PPS current_pps_;
  scoped_refptr<H264BitstreamBuffer> packed_pps_;
  bool submit_packed_headers_;

  // Current encoding parameters being used.
  EncodeParams curr_params_;

  // H264 profile currently used.
  VideoCodecProfile profile_ = VIDEO_CODEC_PROFILE_UNKNOWN;

  // H264 level currently used.
  uint8_t level_ = 0;

  // Current visible and coded sizes in pixels.
  gfx::Size visible_size_;
  gfx::Size coded_size_;

  // Width/height in macroblocks.
  unsigned int mb_width_ = 0;
  unsigned int mb_height_ = 0;

  // The number of encoded frames. Resets to 0 on IDR frame.
  unsigned int num_encoded_frames_ = 0;
  // frame_num (spec section 7.4.3).
  unsigned int frame_num_ = 0;

  // idr_pic_id (spec section 7.4.3) to be used for the next frame.
  unsigned int idr_pic_id_ = 0;

  // True if encoding parameters have changed that affect decoder process, then
  // we need to submit a keyframe with updated parameters.
  bool encoding_parameters_changed_ = false;

  // Currently active reference frames.
  // RefPicList0 per spec (spec section 8.2.4.2).
  base::circular_deque<scoped_refptr<H264Picture>> ref_pic_list0_;

  // Sets true if and only if testing.
  // TODO(b/199487660): Remove once all drivers support temporal layers.
  bool supports_temporal_layer_for_testing_ = false;

  uint8_t num_temporal_layers_ = 1;
};

}  // namespace media

#endif  // SRC_MEDIA_CODEC_CODECS_VAAPI_THIRD_PARTY_CHROMIUM_H264_VAAPI_VIDEO_ENCODER_DELEGATE_H_
