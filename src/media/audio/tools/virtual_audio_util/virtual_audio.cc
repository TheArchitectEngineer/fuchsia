// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be found in the LICENSE file.

#include <fuchsia/virtualaudio/cpp/fidl.h>
#include <lib/async-loop/cpp/loop.h>
#include <lib/async-loop/default.h>
#include <lib/async/cpp/task.h>
#include <lib/fdio/directory.h>
#include <lib/fzl/vmo-mapper.h>
#include <lib/media/cpp/timeline_function.h>
#include <lib/media/cpp/timeline_rate.h>
#include <lib/sys/cpp/component_context.h>
#include <lib/syslog/cpp/log_settings.h>
#include <lib/syslog/cpp/macros.h>
#include <lib/zx/clock.h>
#include <poll.h>
#include <unistd.h>
#include <zircon/device/audio.h>
#include <zircon/status.h>
#include <zircon/syscalls/clock.h>

#include <cstddef>
#include <iterator>
#include <limits>
#include <optional>

#include <fbl/algorithm.h>

#include "src/lib/fsl/tasks/fd_waiter.h"
#include "src/lib/fxl/command_line.h"
#include "src/lib/fxl/strings/string_number_conversions.h"

namespace {
inline std::string to_string(std::optional<bool> selector, const std::string& true_str,
                             const std::string& false_str, const std::string& none_str) {
  if (selector.has_value()) {
    return *selector ? true_str : false_str;
  }
  return none_str;
}
}  // namespace

namespace virtual_audio {

class VirtualAudioUtil {
 public:
  explicit VirtualAudioUtil(async::Loop* loop) { VirtualAudioUtil::loop_ = loop; }

  void Run(fxl::CommandLine* cmdline);

 private:
  enum class Command : uint8_t {
    GET_NUM_VIRTUAL_DEVICES,

    SET_DEVICE_NAME,
    SET_MANUFACTURER,
    SET_PRODUCT_NAME,
    SET_UNIQUE_ID,
    ADD_FORMAT_RANGE,
    CLEAR_FORMAT_RANGES,
    SET_CLOCK_DOMAIN,
    SET_INITIAL_CLOCK_RATE,
    SET_TRANSFER_BYTES,
    SET_INTERNAL_DELAY,
    SET_EXTERNAL_DELAY,
    SET_RING_BUFFER_RESTRICTIONS,
    SET_GAIN_PROPS,
    SET_PLUG_PROPS,
    RESET_CONFIG,

    ADD_DEVICE,
    REMOVE_DEVICE,
    PLUG,
    UNPLUG,
    GET_GAIN,
    GET_FORMAT,
    RETRIEVE_BUFFER,
    WRITE_BUFFER,
    GET_POSITION,
    SET_NOTIFICATION_FREQUENCY,
    ADJUST_CLOCK_RATE,

    SET_CODEC,
    SET_COMPOSITE,
    SET_DAI,
    SET_STREAM_CONFIG,
    SET_IN,
    SET_OUT,
    SET_NO_DIRECTION,
    WAIT,
    HELP,
    INVALID,
  };

  static constexpr char kNumDevsSwitch[] = "num-devs";

  static constexpr char kDeviceNameSwitch[] = "dev";
  static constexpr char kManufacturerSwitch[] = "mfg";
  static constexpr char kProductNameSwitch[] = "prod";
  static constexpr char kUniqueIdSwitch[] = "id";
  static constexpr char kAddFormatRangeSwitch[] = "add-format";
  static constexpr char kClearFormatRangesSwitch[] = "clear-format";
  static constexpr char kClockDomainSwitch[] = "domain";
  static constexpr char kInitialRateSwitch[] = "initial-rate";
  static constexpr char kTransferBytesSwitch[] = "transfer";
  static constexpr char kInternalDelaySwitch[] = "int-delay";
  static constexpr char kExternalDelaySwitch[] = "ext-delay";
  static constexpr char kBufferRestrictionsSwitch[] = "rb";
  static constexpr char kGainPropsSwitch[] = "gain-props";
  static constexpr char kPlugPropsSwitch[] = "plug-props";
  static constexpr char kResetConfigSwitch[] = "reset";

  static constexpr char kAddDeviceSwitch[] = "add";
  static constexpr char kRemoveDeviceSwitch[] = "remove";

  static constexpr char kPlugSwitch[] = "plug";
  static constexpr char kUnplugSwitch[] = "unplug";
  static constexpr char kGetGainSwitch[] = "get-gain";
  static constexpr char kGetFormatSwitch[] = "get-format";
  static constexpr char kRetrieveBufferSwitch[] = "get-rb";
  static constexpr char kWriteBufferSwitch[] = "write-rb";
  static constexpr char kGetPositionSwitch[] = "get-pos";
  static constexpr char kNotificationFrequencySwitch[] = "notifs";
  static constexpr char kClockRateSwitch[] = "rate";

  static constexpr char kCodecSwitch[] = "codec";
  static constexpr char kCompositeSwitch[] = "composite";
  static constexpr char kDaiSwitch[] = "dai";
  static constexpr char kStreamConfigSwitch[] = "stream";
  static constexpr char kDirectionInSwitch[] = "in";
  static constexpr char kDirectionOutSwitch[] = "out";
  static constexpr char kDirectionlessSwitch[] = "no-direction";
  static constexpr char kWaitSwitch[] = "wait";
  static constexpr char kHelp1Switch[] = "help";
  static constexpr char kHelp2Switch[] = "?";

  static constexpr char kDefaultDeviceName[] = "Vertex";
  static constexpr char kDefaultManufacturer[] = "Puerile Virtual Functions, Incorporated";
  static constexpr char kDefaultProductName[] = "Virgil, version 1.0";
  static constexpr uint8_t kDefaultUniqueId[16] = {0x01, 0x23, 0x45, 0x67, 0x89, 0xAB, 0xCD, 0xEF,
                                                   0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88};

  static constexpr int32_t kDefaultClockDomain = 0;
  static constexpr int32_t kDefaultInitialClockRatePpm = 0;

  static constexpr uint8_t kDefaultFormatRangeOption = 0;

  static constexpr uint32_t kDefaultTransferBytes = 0x100;
  static constexpr int64_t kDefaultInternalDelayNsec = zx::msec(0).get();
  static constexpr int64_t kDefaultExternalDelayNsec = zx::msec(1).get();
  static constexpr uint8_t kDefaultRingBufferOption = 0;

  // This repeated value can be interpreted various ways, at various sample_sizes and num_chans.
  static constexpr uint64_t kDefaultValueToWrite = 0x22446688AACCEE00;

  static constexpr uint8_t kDefaultGainPropsOption = 0;
  static constexpr uint8_t kDefaultPlugPropsOption = 0;
  static constexpr uint32_t kDefaultNotificationFrequency = 4;

  static constexpr struct {
    const char* name;
    Command cmd;
  } COMMANDS[] = {
      {.name = kNumDevsSwitch, .cmd = Command::GET_NUM_VIRTUAL_DEVICES},

      {.name = kDeviceNameSwitch, .cmd = Command::SET_DEVICE_NAME},
      {.name = kManufacturerSwitch, .cmd = Command::SET_MANUFACTURER},
      {.name = kProductNameSwitch, .cmd = Command::SET_PRODUCT_NAME},
      {.name = kUniqueIdSwitch, .cmd = Command::SET_UNIQUE_ID},
      {.name = kAddFormatRangeSwitch, .cmd = Command::ADD_FORMAT_RANGE},
      {.name = kClearFormatRangesSwitch, .cmd = Command::CLEAR_FORMAT_RANGES},
      {.name = kClockDomainSwitch, .cmd = Command::SET_CLOCK_DOMAIN},
      {.name = kInitialRateSwitch, .cmd = Command::SET_INITIAL_CLOCK_RATE},
      {.name = kTransferBytesSwitch, .cmd = Command::SET_TRANSFER_BYTES},
      {.name = kInternalDelaySwitch, .cmd = Command::SET_INTERNAL_DELAY},
      {.name = kExternalDelaySwitch, .cmd = Command::SET_EXTERNAL_DELAY},
      {.name = kBufferRestrictionsSwitch, .cmd = Command::SET_RING_BUFFER_RESTRICTIONS},
      {.name = kGainPropsSwitch, .cmd = Command::SET_GAIN_PROPS},
      {.name = kPlugPropsSwitch, .cmd = Command::SET_PLUG_PROPS},
      {.name = kResetConfigSwitch, .cmd = Command::RESET_CONFIG},

      {.name = kAddDeviceSwitch, .cmd = Command::ADD_DEVICE},
      {.name = kRemoveDeviceSwitch, .cmd = Command::REMOVE_DEVICE},

      {.name = kPlugSwitch, .cmd = Command::PLUG},
      {.name = kUnplugSwitch, .cmd = Command::UNPLUG},
      {.name = kGetGainSwitch, .cmd = Command::GET_GAIN},
      {.name = kGetFormatSwitch, .cmd = Command::GET_FORMAT},
      {.name = kRetrieveBufferSwitch, .cmd = Command::RETRIEVE_BUFFER},
      {.name = kWriteBufferSwitch, .cmd = Command::WRITE_BUFFER},
      {.name = kGetPositionSwitch, .cmd = Command::GET_POSITION},
      {.name = kNotificationFrequencySwitch, .cmd = Command::SET_NOTIFICATION_FREQUENCY},
      {.name = kClockRateSwitch, .cmd = Command::ADJUST_CLOCK_RATE},

      {.name = kCodecSwitch, .cmd = Command::SET_CODEC},
      {.name = kCompositeSwitch, .cmd = Command::SET_COMPOSITE},
      {.name = kDaiSwitch, .cmd = Command::SET_DAI},
      {.name = kStreamConfigSwitch, .cmd = Command::SET_STREAM_CONFIG},
      {.name = kDirectionInSwitch, .cmd = Command::SET_IN},
      {.name = kDirectionOutSwitch, .cmd = Command::SET_OUT},
      {.name = kDirectionlessSwitch, .cmd = Command::SET_NO_DIRECTION},
      {.name = kWaitSwitch, .cmd = Command::WAIT},
      {.name = kHelp1Switch, .cmd = Command::HELP},
      {.name = kHelp2Switch, .cmd = Command::HELP},
  };

  static async::Loop* loop_;
  static bool received_callback_;

  static void QuitLoop();
  static bool RunForDuration(zx::duration duration);
  static bool WaitForNoCallback();
  static bool WaitForCallback();

  void RegisterKeyWaiter();
  bool WaitForKey();

  bool ConnectToControllers();
  bool ConnectToDevice();
  void SetUpEvents();

  void ParseAndExecute(fxl::CommandLine* cmdline);
  bool ExecuteCommand(Command cmd, const std::string& value);
  static void Usage();

  // Methods using the FIDL Service interface
  bool GetNumDevices();
  bool AddDevice();

  // Methods using the FIDL Configuration interface
  bool SetDeviceName(const std::string& name);
  bool SetManufacturer(const std::string& name);
  bool SetProductName(const std::string& name);
  bool SetUniqueId(const std::string& unique_id);
  bool AddFormatRange(const std::string& format_range_str);
  bool ClearFormatRanges();
  bool SetClockDomain(const std::string& clock_domain_str);
  bool SetInitialClockRate(const std::string& initial_clock_rate_str);
  bool SetTransferBytes(const std::string& transfer_bytes_str);
  bool SetInternalDelay(const std::string& delay_str);
  bool SetExternalDelay(const std::string& delay_str);
  bool SetRingBufferRestrictions(const std::string& rb_restr_str);
  bool SetGainProps(const std::string& gain_props_str);
  bool SetPlugProps(const std::string& plug_props_str);
  zx_status_t ResetConfiguration(fuchsia::virtualaudio::DeviceType device_type,
                                 std::optional<bool> is_input);
  bool ResetAllConfigurations();

  // Methods using the FIDL Device interface
  bool RemoveDevice();
  bool ChangePlugState(const std::string& plug_time_str, bool plugged);
  bool GetGain();
  bool GetFormat();
  bool GetBuffer();
  bool WriteBuffer(const std::string& write_value_str);
  bool GetPosition();
  bool SetNotificationFrequency(const std::string& override_notifs_str);
  bool AdjustClockRate(const std::string& clock_adjust_str);
  bool SetDirection(std::optional<bool> is_input);

  // Convenience method that allows us to set configuration without having to check
  // that some FIDL table members have been defined.
  void EnsureTypesExist();

  // Get the virtual-audio controller that supports the given device type.
  fuchsia::virtualaudio::ControlSyncPtr& GetController(
      fuchsia::virtualaudio::DeviceType device_type);
  fuchsia::virtualaudio::ControlSyncPtr& GetController(
      fuchsia::virtualaudio::DeviceSpecific::Tag device_type);

  std::unique_ptr<sys::ComponentContext> component_context_;
  fsl::FDWaiter keystroke_waiter_;
  bool key_quit_ = false;

  fuchsia::virtualaudio::ControlSyncPtr controller_ = nullptr;
  fuchsia::virtualaudio::ControlSyncPtr legacy_controller_ = nullptr;

  fuchsia::virtualaudio::DevicePtr codec_ = nullptr;
  fuchsia::virtualaudio::DevicePtr codec_input_ = nullptr;
  fuchsia::virtualaudio::DevicePtr codec_output_ = nullptr;
  fuchsia::virtualaudio::DevicePtr composite_ = nullptr;
  fuchsia::virtualaudio::DevicePtr dai_input_ = nullptr;
  fuchsia::virtualaudio::DevicePtr dai_output_ = nullptr;
  fuchsia::virtualaudio::DevicePtr stream_config_input_ = nullptr;
  fuchsia::virtualaudio::DevicePtr stream_config_output_ = nullptr;

  fuchsia::virtualaudio::Configuration codec_config_;
  fuchsia::virtualaudio::Configuration codec_input_config_;
  fuchsia::virtualaudio::Configuration codec_output_config_;
  fuchsia::virtualaudio::Configuration composite_config_;
  fuchsia::virtualaudio::Configuration dai_input_config_;
  fuchsia::virtualaudio::Configuration dai_output_config_;
  fuchsia::virtualaudio::Configuration stream_config_input_config_;
  fuchsia::virtualaudio::Configuration stream_config_output_config_;

  std::optional<bool> configuring_input_;  // Not applicable for Composite devices.
  static zx::vmo ring_buffer_vmo_;

  static uint32_t BytesPerSample(uint32_t format);
  static void UpdateRunningPosition(uint32_t ring_position, bool is_output_);

  static size_t rb_size_[2];
  static uint32_t last_rb_position_[2];
  static uint64_t running_position_[2];

 public:
  static uint32_t frame_size_[2];
  static media::TimelineRate ref_time_to_running_position_rate_[2];
  static media::TimelineFunction ref_time_to_running_position_[2];

 private:
  fuchsia::virtualaudio::DevicePtr* device() {
    switch (config()->device_specific().Which()) {
      case fuchsia::virtualaudio::DeviceSpecific::Tag::kCodec:
        if (configuring_input_.has_value()) {
          return (*configuring_input_ ? &codec_input_ : &codec_output_);
        }
        return &codec_;
      case fuchsia::virtualaudio::DeviceSpecific::Tag::kComposite:
        return &composite_;
      case fuchsia::virtualaudio::DeviceSpecific::Tag::kDai:
        return configuring_input_.value_or(false) ? &dai_input_ : &dai_output_;
      case fuchsia::virtualaudio::DeviceSpecific::Tag::kStreamConfig:
        return configuring_input_.value_or(false) ? &stream_config_input_ : &stream_config_output_;
      default:
        ZX_ASSERT_MSG(0, "Unknown device type");
    }
  }
  fuchsia::virtualaudio::Configuration* ConfigForDevice(
      std::optional<bool> is_input, fuchsia::virtualaudio::DeviceType device_type) {
    switch (device_type) {
      case fuchsia::virtualaudio::DeviceType::CODEC:
        if (is_input.has_value()) {
          return (*is_input ? &codec_input_config_ : &codec_output_config_);
        }
        return &codec_config_;
      case fuchsia::virtualaudio::DeviceType::COMPOSITE:
        return &composite_config_;
      case fuchsia::virtualaudio::DeviceType::DAI:
        return is_input.value_or(false) ? &dai_input_config_ : &dai_output_config_;
      case fuchsia::virtualaudio::DeviceType::STREAM_CONFIG:
        return is_input.value_or(false) ? &stream_config_input_config_
                                        : &stream_config_output_config_;
      default:
        ZX_ASSERT_MSG(0, "Unknown device type");
    }
  }
  fuchsia::virtualaudio::Configuration* config() {
    return ConfigForDevice(configuring_input_, device_type_);
  }

  static void CallbackReceived();
  template <bool is_out>
  static void FormatNotification(uint32_t fps, uint32_t fmt, uint32_t chans, zx_duration_t delay);
  template <bool is_out>
  static void FormatCallback(fuchsia::virtualaudio::Device_GetFormat_Result result);

  template <bool is_out>
  static void GainNotification(bool current_mute, bool current_agc, float gain_db);
  template <bool is_out>
  static void GainCallback(fuchsia::virtualaudio::Device_GetGain_Result result);

  template <bool is_out>
  static void BufferNotification(zx::vmo ring_buffer_vmo, uint32_t num_ring_buffer_frames,
                                 uint32_t notifications_per_ring);
  template <bool is_out>
  static void BufferCallback(fuchsia::virtualaudio::Device_GetBuffer_Result result);

  template <bool is_out>
  static void StartNotification(zx_time_t start_time);
  template <bool is_out>
  static void StopNotification(zx_time_t stop_time, uint32_t ring_position);

  template <bool is_out>
  static void PositionNotification(zx_time_t monotonic_time_for_position, uint32_t ring_position);
  template <bool is_out>
  static void PositionCallback(fuchsia::virtualaudio::Device_GetPosition_Result result);

  fuchsia::virtualaudio::DeviceType device_type_ = fuchsia::virtualaudio::DeviceType::COMPOSITE;
};

::async::Loop* VirtualAudioUtil::loop_;
bool VirtualAudioUtil::received_callback_;
zx::vmo VirtualAudioUtil::ring_buffer_vmo_;

size_t VirtualAudioUtil::rb_size_[2];
uint32_t VirtualAudioUtil::last_rb_position_[2];
uint64_t VirtualAudioUtil::running_position_[2];
uint32_t VirtualAudioUtil::frame_size_[2];
media::TimelineRate VirtualAudioUtil::ref_time_to_running_position_rate_[2];
media::TimelineFunction VirtualAudioUtil::ref_time_to_running_position_[2];

enum DeviceDirection : uint8_t { kOutput = 0u, kInput = 1u };
uint32_t VirtualAudioUtil::BytesPerSample(uint32_t format_bitfield) {
  if (format_bitfield & (AUDIO_SAMPLE_FORMAT_20BIT_IN32 | AUDIO_SAMPLE_FORMAT_24BIT_IN32 |
                         AUDIO_SAMPLE_FORMAT_32BIT | AUDIO_SAMPLE_FORMAT_32BIT_FLOAT)) {
    return 4;
  }
  if (format_bitfield & AUDIO_SAMPLE_FORMAT_24BIT_PACKED) {
    return 3;
  }
  if (format_bitfield & AUDIO_SAMPLE_FORMAT_16BIT) {
    return 2;
  }
  if (format_bitfield & AUDIO_SAMPLE_FORMAT_8BIT) {
    return 1;
  }

  printf("\n--Unknown format, could not determine bytes per sample. Exiting.\n");

  return 0;
}

// VirtualAudioUtil implementation
//
void VirtualAudioUtil::Run(fxl::CommandLine* cmdline) {
  ParseAndExecute(cmdline);

  // We are done!  Disconnect any error handlers.
  codec_.set_error_handler(nullptr);
  codec_input_.set_error_handler(nullptr);
  codec_output_.set_error_handler(nullptr);
  composite_.set_error_handler(nullptr);
  dai_input_.set_error_handler(nullptr);
  dai_output_.set_error_handler(nullptr);
  stream_config_input_.set_error_handler(nullptr);
  stream_config_output_.set_error_handler(nullptr);

  // If any lingering callbacks were queued, let them drain.
  if (!WaitForNoCallback()) {
    printf("Received unexpected callback!\n");
  }
}

void VirtualAudioUtil::QuitLoop() {
  async::PostTask(loop_->dispatcher(), [loop = loop_]() { loop->Quit(); });
}

// Below was borrowed from gtest, as-is
bool VirtualAudioUtil::RunForDuration(zx::duration duration) {
  auto canceled = std::make_shared<bool>(false);
  bool timed_out = false;
  async::PostDelayedTask(
      loop_->dispatcher(),
      [loop = loop_, canceled, &timed_out] {
        if (*canceled) {
          return;
        }
        timed_out = true;
        loop->Quit();
      },
      duration);
  loop_->Run();
  loop_->ResetQuit();

  if (!timed_out) {
    *canceled = true;
  }
  return timed_out;
}
// Above was borrowed from gtest, as-is

bool VirtualAudioUtil::WaitForNoCallback() {
  received_callback_ = false;
  bool timed_out = RunForDuration(zx::msec(5));

  // If all is well, we DIDN'T get a disconnect callback and are still bound.
  if (received_callback_) {
    printf("  ... received unexpected callback\n");
  }
  return (timed_out && !received_callback_);
}

bool VirtualAudioUtil::WaitForCallback() {
  received_callback_ = false;
  bool timed_out = RunForDuration(zx::msec(2000));

  if (!received_callback_) {
    printf("  ... expected a callback; none was received\n");
  }
  return (!timed_out && received_callback_);
}

void VirtualAudioUtil::RegisterKeyWaiter() {
  keystroke_waiter_.Wait(
      [this](zx_status_t, uint32_t) {
        int c = std::tolower(getc(stdin));
        if (c == 'q') {
          key_quit_ = true;
        }
        QuitLoop();
      },
      STDIN_FILENO, POLLIN);
}

bool VirtualAudioUtil::WaitForKey() {
  printf("\tPress Q to cancel, or any other key to continue...\n");
  setvbuf(stdin, nullptr, _IONBF, 0); // Turn off buffering; immediately receive keypresses.
  RegisterKeyWaiter();

  while (RunForDuration(zx::sec(1))) {
  }

  return !key_quit_;
}

bool VirtualAudioUtil::ConnectToControllers() {
  const std::string kControlNodePath =
      std::string{"/dev/"} + fuchsia::virtualaudio::CONTROL_NODE_NAME;
  zx_status_t status = fdio_service_connect(kControlNodePath.c_str(),
                                            controller_.NewRequest().TakeChannel().release());
  if (status != ZX_OK) {
    printf("ERROR: failed to connect to '%s', status = %d\n", kControlNodePath.c_str(), status);
    return false;
  }

  // let VirtualAudio disconnect if all is not well.
  bool success = (WaitForNoCallback() && controller_.is_bound());
  if (!success) {
    printf("Failed to establish channel to async controller\n");
    return false;
  }

  const std::string kLegacyControlNodePath =
      std::string{"/dev/"} + fuchsia::virtualaudio::LEGACY_CONTROL_NODE_NAME;
  status = fdio_service_connect(kLegacyControlNodePath.c_str(),
                                legacy_controller_.NewRequest().TakeChannel().release());
  if (status != ZX_OK) {
    printf("ERROR: failed to connect to '%s', status = %d\n", kLegacyControlNodePath.c_str(),
           status);
    return false;
  }

  // let VirtualAudio disconnect if all is not well.
  success = (WaitForNoCallback() && legacy_controller_.is_bound());
  if (!success) {
    printf("Failed to establish channel to async legacy controller\n");
    return false;
  }

  return true;
}

void VirtualAudioUtil::SetUpEvents() {
  if (configuring_input_) {
    device()->events().OnSetFormat = FormatNotification<false>;
    device()->events().OnSetGain = GainNotification<false>;
    device()->events().OnBufferCreated = BufferNotification<false>;
    device()->events().OnStart = StartNotification<false>;
    device()->events().OnStop = StopNotification<false>;
    device()->events().OnPositionNotify = PositionNotification<false>;
  } else {
    device()->events().OnSetFormat = FormatNotification<true>;
    device()->events().OnSetGain = GainNotification<true>;
    device()->events().OnBufferCreated = BufferNotification<true>;
    device()->events().OnStart = StartNotification<true>;
    device()->events().OnStop = StopNotification<true>;
    device()->events().OnPositionNotify = PositionNotification<true>;
  }
}

bool VirtualAudioUtil::ResetAllConfigurations() {
  if (ResetConfiguration(fuchsia::virtualaudio::DeviceType::COMPOSITE, std::nullopt) != ZX_OK) {
    QuitLoop();
    return false;
  }

  if (ResetConfiguration(fuchsia::virtualaudio::DeviceType::CODEC, true) != ZX_OK ||
      ResetConfiguration(fuchsia::virtualaudio::DeviceType::CODEC, false) != ZX_OK ||
      ResetConfiguration(fuchsia::virtualaudio::DeviceType::CODEC, std::nullopt) != ZX_OK) {
    printf("ERROR: failed to ResetConfiguration for Codec, but continuing anyway...\n");
  }

  if (ResetConfiguration(fuchsia::virtualaudio::DeviceType::DAI, true) != ZX_OK ||
      ResetConfiguration(fuchsia::virtualaudio::DeviceType::DAI, false) != ZX_OK) {
    printf("ERROR: failed to ResetConfiguration for Dai, but continuing anyway...\n");
  }

  if (ResetConfiguration(fuchsia::virtualaudio::DeviceType::STREAM_CONFIG, true) != ZX_OK ||
      ResetConfiguration(fuchsia::virtualaudio::DeviceType::STREAM_CONFIG, false) != ZX_OK) {
    printf("ERROR: failed to ResetConfiguration for StreamConfig, but continuing anyway...\n");
  }

  return true;
}

void VirtualAudioUtil::ParseAndExecute(fxl::CommandLine* cmdline) {
  if (!cmdline->has_argv0() || cmdline->options().empty()) {
    printf("No commands provided; no action taken\n");
    return;
  }

  // Looks like we will interact with the service; get ready to connect to it.
  component_context_ = sys::ComponentContext::CreateAndServeOutgoingDirectory();

  if (!ConnectToControllers()) {
    return;
  }

  if (!ResetAllConfigurations()) {
    return;
  }

  // Defaults are Composite.
  device_type_ = fuchsia::virtualaudio::DeviceType::COMPOSITE;
  configuring_input_ = false;

  for (const auto& option : cmdline->options()) {
    bool success = false;
    Command cmd = Command::INVALID;

    for (const auto& entry : COMMANDS) {
      if (option.name == entry.name) {
        cmd = entry.cmd;
        success = true;

        break;
      }
    }

    if (!success) {
      printf("Failed to parse command ID `--%s'\n", option.name.c_str());
      Usage();
      return;
    }

    printf("Executing `--%s' command...\n", option.name.c_str());
    success = ExecuteCommand(cmd, option.value);
    if (!success) {
      printf("  ... `--%s' command was unsuccessful\n", option.name.c_str());
      return;
    }
  }  // while (cmdline args) without default
}

bool VirtualAudioUtil::ExecuteCommand(Command cmd, const std::string& value) {
  bool success;
  switch (cmd) {
    // FIDL Service methods
    case Command::GET_NUM_VIRTUAL_DEVICES:
      success = GetNumDevices();
      break;

    // FIDL Configuration/Device methods
    case Command::SET_DEVICE_NAME:
      success = SetDeviceName(value);
      break;
    case Command::SET_MANUFACTURER:
      success = SetManufacturer(value);
      break;
    case Command::SET_PRODUCT_NAME:
      success = SetProductName(value);
      break;
    case Command::SET_UNIQUE_ID:
      success = SetUniqueId(value);
      break;
    case Command::SET_CLOCK_DOMAIN:
      success = SetClockDomain(value);
      break;
    case Command::SET_INITIAL_CLOCK_RATE:
      success = SetInitialClockRate(value);
      break;
    case Command::ADD_FORMAT_RANGE:
      success = AddFormatRange(value);
      break;
    case Command::CLEAR_FORMAT_RANGES:
      success = ClearFormatRanges();
      break;
    case Command::SET_TRANSFER_BYTES:
      success = SetTransferBytes(value);
      break;
    case Command::SET_INTERNAL_DELAY:
      success = SetInternalDelay(value);
      break;
    case Command::SET_EXTERNAL_DELAY:
      success = SetExternalDelay(value);
      break;
    case Command::SET_RING_BUFFER_RESTRICTIONS:
      success = SetRingBufferRestrictions(value);
      break;
    case Command::SET_GAIN_PROPS:
      success = SetGainProps(value);
      break;
    case Command::SET_PLUG_PROPS:
      success = SetPlugProps(value);
      break;
    case Command::RESET_CONFIG:
      success = (ResetConfiguration(device_type_, configuring_input_) == ZX_OK);
      break;

    case Command::ADD_DEVICE:
      success = AddDevice();
      break;
    case Command::REMOVE_DEVICE:
      success = RemoveDevice();
      break;

    case Command::PLUG:
      success = ChangePlugState(value, true);
      break;
    case Command::UNPLUG:
      success = ChangePlugState(value, false);
      break;
    case Command::GET_GAIN:
      success = GetGain();
      break;
    case Command::GET_FORMAT:
      success = GetFormat();
      break;
    case Command::RETRIEVE_BUFFER:
      success = GetBuffer();
      break;
    case Command::WRITE_BUFFER:
      success = WriteBuffer(value);
      break;
    case Command::GET_POSITION:
      success = GetPosition();
      break;
    case Command::SET_NOTIFICATION_FREQUENCY:
      success = SetNotificationFrequency(value);
      break;
    case Command::ADJUST_CLOCK_RATE:
      success = AdjustClockRate(value);
      break;

    case Command::SET_CODEC:
      device_type_ = fuchsia::virtualaudio::DeviceType::CODEC;
      success = true;
      break;
    case Command::SET_COMPOSITE:
      device_type_ = fuchsia::virtualaudio::DeviceType::COMPOSITE;
      success = true;
      break;
    case Command::SET_DAI:
      device_type_ = fuchsia::virtualaudio::DeviceType::DAI;
      success = true;
      break;
    case Command::SET_STREAM_CONFIG:
      device_type_ = fuchsia::virtualaudio::DeviceType::STREAM_CONFIG;
      success = true;
      break;
    case Command::SET_IN:
      success = SetDirection(true);
      break;
    case Command::SET_OUT:
      success = SetDirection(false);
      break;
    case Command::SET_NO_DIRECTION:
      success = SetDirection(std::nullopt);
      break;
    case Command::WAIT:
      success = WaitForKey();
      break;
    case Command::HELP:
      Usage();
      success = true;
      break;
    case Command::INVALID:
      success = false;
      break;

      // Intentionally omitting default, so new enums are not forgotten here.
  }
  return success;
}

void VirtualAudioUtil::Usage() {
  printf("\nUsage: virtual_audio [options]\n");
  printf("Interactively configure and control virtual audio devices.\n");

  printf("\nValid options:\n");

  printf("\n  By default, a virtual device of type Composite is used\n");
  printf("  --%s    \t\t  Switch to a Codec configuration with the same direction\n", kCodecSwitch);
  printf("  --%s\t\t  Switch to a Composite configuration with the same direction\n",
         kCompositeSwitch);
  printf("  --%s      \t\t  Switch to a Dai configuration with the same direction\n", kDaiSwitch);
  printf("  --%s   \t\t  Switch to a StreamConfig configuration with the same direction\n",
         kStreamConfigSwitch);
  printf("  --%s\t\t\t  Switch to an Input configuration (same device type)\n", kDirectionInSwitch);
  printf("  --%s\t\t\t  Switch to an Output configuration (same device type)\n",
         kDirectionOutSwitch);
  printf("  --%s\t  Switch to a direction-less configuration (same device type)\n",
         kDirectionlessSwitch);

  printf("\n  The following commands customize a device configuration, before it is added\n");
  printf("  --%s[=<DEVICE_NAME>]\t  Set the device name (default '%s')\n", kDeviceNameSwitch,
         kDefaultDeviceName);
  printf("  --%s[=<MANUFACTURER>]  Set the manufacturer name (default '%s')\n", kManufacturerSwitch,
         kDefaultManufacturer);
  printf("  --%s[=<PRODUCT>]\t  Set the product name (default '%s')\n", kProductNameSwitch,
         kDefaultProductName);
  printf(
      "  --%s[=<UINT128>]\t  Set the unique ID (default %02X%02X%02X%02X%02X%02X%02X%02X%02X%02X%02X%02X%02X%02X%02X%02X)\n",
      kUniqueIdSwitch, kDefaultUniqueId[0], kDefaultUniqueId[1], kDefaultUniqueId[2],
      kDefaultUniqueId[3], kDefaultUniqueId[4], kDefaultUniqueId[5], kDefaultUniqueId[6],
      kDefaultUniqueId[7], kDefaultUniqueId[8], kDefaultUniqueId[9], kDefaultUniqueId[10],
      kDefaultUniqueId[11], kDefaultUniqueId[12], kDefaultUniqueId[13], kDefaultUniqueId[14],
      kDefaultUniqueId[15]);
  printf("  --%s[=<NUM>]\t  Add format range [0,6] (default 8-44.1 Mono/Stereo 24-32)\n",
         kAddFormatRangeSwitch);
  printf("  --%s\t  Clear any format ranges (including the built-in default)\n",
         kClearFormatRangesSwitch);
  printf("  --%s[=<NUM>]\t  Set device clock domain (default %d)\n", kClockDomainSwitch,
         kDefaultClockDomain);
  printf("  --%s[=<NUM>]  Set initial device clock rate in PPM [-1000, 1000] (default %d)\n",
         kInitialRateSwitch, kDefaultInitialClockRatePpm);
  printf("  --%s[=<BYTES>]\t  Set the transfer bytes, in bytes (default %u)\n",
         kTransferBytesSwitch, kDefaultTransferBytes);

  printf("  --%s[=<NSEC>]\t  Set internal delay (default %zd ns)\n", kInternalDelaySwitch,
         kDefaultInternalDelayNsec);
  printf("  --%s[=<NSEC>]\t  Set external delay (default %zd ns)\n", kExternalDelaySwitch,
         kDefaultExternalDelayNsec);
  printf("  --%s[=<NUM>]\t\t  Set ring-buffer restrictions [0,2] (default 48k-72k frames mod 6k)\n",
         kBufferRestrictionsSwitch);
  printf("  --%s[=<NUM>]\t  Set gain properties [0,3] (default [-60, 0] -2dB mute)\n",
         kGainPropsSwitch);
  printf("  --%s[=<NUM>]\t  Set plug properties [0,5] (default plugged notifiable)\n",
         kPlugPropsSwitch);
  printf("  --%s\t\t  Clear any customizations; return this configuration to the default\n",
         kResetConfigSwitch);

  printf("\n  --%s\t\t\t  Activate the current configuration (AddDevice)\n", kAddDeviceSwitch);

  printf("\n  Subsequent commands require an activated (added) virtual audio device\n");
  printf("  --%s\t\t  Retrieve the client-selected ring-buffer format\n", kGetFormatSwitch);
  printf("  --%s\t\t  Retrieve the current device gain\n", kGetGainSwitch);
  printf("  --%s\t\t  Return a mapping of the ring buffer\n", kRetrieveBufferSwitch);
  printf(
      "  --%s[=<UINT64>]\t  Fill the ring-buffer with this uint64 (in hex, default "
      "0x%zX)\n",
      kWriteBufferSwitch, kDefaultValueToWrite);
  printf("  --%s\t\t  Retrieve the current ring-buffer position and corresponding ref time\n",
         kGetPositionSwitch);
  printf("  --%s[=<FREQ>]\t  Set an alternate notifications-per-ring frequency (default %u).\n",
         kNotificationFrequencySwitch, kDefaultNotificationFrequency);
  printf("\t\t\t  (Don't receive the same position notifications sent to the client)\n");
  printf("  --%s=<DELTA PPM>\t  Adjust the rate of the device clock, in parts-per-million\n",
         kClockRateSwitch);
  printf("\t\t\t  This is reflected in position notification delivery timing and timestamps.\n");
  printf("  --%s\t\t  Change the device's plug-state to Plugged\n", kPlugSwitch);
  printf("  --%s\t\t  Change the device's plug-state to Unplugged\n", kUnplugSwitch);

  printf("\n  --%s\t\t  Deactivate the current device configuration (RemoveDevice)\n",
         kRemoveDeviceSwitch);

  printf("\n  The following commands are on the virtualaudio::Control protocol:\n");
  printf("  --%s\t\t  Retrieve the number of currently active virtual audio devices\n",
         kNumDevsSwitch);

  printf("\n  --%s\t\t  Wait for a key press before executing subsequent commands\n", kWaitSwitch);
  printf("  --%s, --%s\t\t  Show this message\n", kHelp1Switch, kHelp2Switch);
  printf("\n");
}

bool VirtualAudioUtil::GetNumDevices() {
  uint32_t num_inputs;
  uint32_t num_outputs;
  uint32_t num_unspecified_direction;
  zx_status_t status =
      controller_->GetNumDevices(&num_inputs, &num_outputs, &num_unspecified_direction);
  if (status != ZX_OK) {
    printf("ERROR: GetNumDevices (non-legacy) failed, status = %d\n", status);
    return false;
  }

  uint32_t num_legacy_inputs;
  uint32_t num_legacy_outputs;
  uint32_t num_legacy_unspecified_direction;
  status = legacy_controller_->GetNumDevices(&num_legacy_inputs, &num_legacy_outputs,
                                             &num_legacy_unspecified_direction);
  if (status != ZX_OK) {
    printf("ERROR: GetNumDevices (legacy) failed, status = %d\n", status);
    return false;
  }

  printf(
      "--Received NumDevices (%u inputs, %u legacy inputs, %u outputs, %u legacy outputs, %u unspecified direction, %u legacy unspecified direction)\n",
      num_inputs, num_legacy_inputs, num_outputs, num_legacy_outputs, num_unspecified_direction,
      num_legacy_unspecified_direction);
  return true;
}

bool VirtualAudioUtil::SetDeviceName(const std::string& name) {
  config()->set_device_name(name);
  return true;
}

bool VirtualAudioUtil::SetManufacturer(const std::string& name) {
  config()->set_manufacturer_name(name);
  return true;
}

bool VirtualAudioUtil::SetProductName(const std::string& name) {
  config()->set_product_name(name);
  return true;
}

bool VirtualAudioUtil::SetUniqueId(const std::string& unique_id_str) {
  std::array<uint8_t, 16> unique_id;
  bool use_default = (unique_id_str.empty());

  for (size_t index = 0; index < 16; ++index) {
    uint8_t val;
    if (use_default) {
      val = kDefaultUniqueId[index];
    } else {
      val = unique_id_str.size() <= (2 * index + 1)
                ? 0
                : fxl::StringToNumber<uint8_t>(unique_id_str.substr(index * 2, 2), fxl::Base::k16);
    }
    unique_id[index] = val;
  }

  memcpy(config()->mutable_unique_id()->data(), unique_id.data(), sizeof(unique_id));
  return true;
}

bool VirtualAudioUtil::SetClockDomain(const std::string& clock_domain_str) {
  int32_t clock_domain =
      (clock_domain_str.empty() ? kDefaultClockDomain
                                : fxl::StringToNumber<int32_t>(clock_domain_str));

  EnsureTypesExist();

  fuchsia::virtualaudio::ClockProperties* clock_properties = nullptr;
  switch (config()->device_specific().Which()) {
    case fuchsia::virtualaudio::DeviceSpecific::Tag::kCodec:
      return false;  // Nothing to do here.
    case fuchsia::virtualaudio::DeviceSpecific::Tag::kComposite:
      clock_properties =
          config()->mutable_device_specific()->composite().mutable_clock_properties();
      break;
    case fuchsia::virtualaudio::DeviceSpecific::Tag::kDai:
      clock_properties = config()->mutable_device_specific()->dai().mutable_clock_properties();
      break;
    case fuchsia::virtualaudio::DeviceSpecific::Tag::kStreamConfig:
      clock_properties =
          config()->mutable_device_specific()->stream_config().mutable_clock_properties();
      break;
    default:
      return false;
  }
  clock_properties->set_domain(clock_domain);

  if (clock_domain == 0 && (clock_properties->has_rate_adjustment_ppm() &&
                            clock_properties->rate_adjustment_ppm() != 0)) {
    printf("WARNING: by definition, a clock in domain 0 should never have rate variance!\n");
  }

  return true;
}

bool VirtualAudioUtil::SetInitialClockRate(const std::string& initial_clock_rate_str) {
  int32_t clock_adjustment_ppm =
      (initial_clock_rate_str.empty() ? kDefaultInitialClockRatePpm
                                      : fxl::StringToNumber<int32_t>(initial_clock_rate_str));
  EnsureTypesExist();

  fuchsia::virtualaudio::ClockProperties* clock_properties = nullptr;
  switch (config()->device_specific().Which()) {
    case fuchsia::virtualaudio::DeviceSpecific::Tag::kCodec:
      return false;  // Nothing to do here.
    case fuchsia::virtualaudio::DeviceSpecific::Tag::kComposite:
      clock_properties =
          config()->mutable_device_specific()->composite().mutable_clock_properties();
      break;
    case fuchsia::virtualaudio::DeviceSpecific::Tag::kDai:
      clock_properties = config()->mutable_device_specific()->dai().mutable_clock_properties();
      break;
    case fuchsia::virtualaudio::DeviceSpecific::Tag::kStreamConfig:
      clock_properties =
          config()->mutable_device_specific()->stream_config().mutable_clock_properties();
      break;
    default:
      return false;
  }
  clock_properties->set_rate_adjustment_ppm(clock_adjustment_ppm);

  if (clock_adjustment_ppm < ZX_CLOCK_UPDATE_MIN_RATE_ADJUST ||
      clock_adjustment_ppm > ZX_CLOCK_UPDATE_MAX_RATE_ADJUST) {
    printf("ERROR: Clock rate adjustment must be within [%d, %d].\n",
           ZX_CLOCK_UPDATE_MIN_RATE_ADJUST, ZX_CLOCK_UPDATE_MAX_RATE_ADJUST);
    return false;
  }
  if ((clock_properties->has_domain() && clock_properties->domain() == 0) &&
      clock_adjustment_ppm != 0) {
    printf("WARNING: by definition, a clock in domain 0 should never have rate variance!\n");
  }

  return true;
}

struct Format {
  uint32_t flags;
  uint32_t min_rate;
  uint32_t max_rate;
  uint8_t min_chans;
  uint8_t max_chans;
  uint16_t rate_family_flags;
};

// These formats exercise various scenarios:
// 0: full range of rates in both families (but not 48k), both 1-2 chans
// 1: float-only, 48k family extends to 96k, 2 or 4 chan
// 2: fixed 48k 2-chan 16b
// 3: 16k 2-chan 16b
// 4: 96k and 48k, 2-chan 16b
// 5: 3-chan device at 48k 16b
// 6: 1-chan device at 8k 16b
// 7: 1-chan device at 48k 16b
// 8: 2-chan device at 96k 16b
//
// Going forward, it would be best to have chans, rate and bitdepth specifiable individually.
constexpr Format kFormatSpecs[9] = {
    {
        .flags = AUDIO_SAMPLE_FORMAT_16BIT | AUDIO_SAMPLE_FORMAT_24BIT_IN32,
        .min_rate = 8000,
        .max_rate = 44100,
        .min_chans = 1,
        .max_chans = 2,
        .rate_family_flags = ASF_RANGE_FLAG_FPS_44100_FAMILY | ASF_RANGE_FLAG_FPS_48000_FAMILY,
    },
    {
        .flags = AUDIO_SAMPLE_FORMAT_32BIT_FLOAT,
        .min_rate = 32000,
        .max_rate = 96000,
        .min_chans = 2,
        .max_chans = 4,
        .rate_family_flags = ASF_RANGE_FLAG_FPS_48000_FAMILY,
    },
    {
        .flags = AUDIO_SAMPLE_FORMAT_16BIT,
        .min_rate = 48000,
        .max_rate = 48000,
        .min_chans = 2,
        .max_chans = 2,
        .rate_family_flags = ASF_RANGE_FLAG_FPS_CONTINUOUS,
    },
    {
        .flags = AUDIO_SAMPLE_FORMAT_16BIT,
        .min_rate = 16000,
        .max_rate = 16000,
        .min_chans = 2,
        .max_chans = 2,
        .rate_family_flags = ASF_RANGE_FLAG_FPS_48000_FAMILY,
    },
    {
        .flags = AUDIO_SAMPLE_FORMAT_16BIT,
        .min_rate = 48000,
        .max_rate = 96000,
        .min_chans = 2,
        .max_chans = 2,
        .rate_family_flags = ASF_RANGE_FLAG_FPS_48000_FAMILY,
    },
    {
        .flags = AUDIO_SAMPLE_FORMAT_16BIT,
        .min_rate = 48000,
        .max_rate = 48000,
        .min_chans = 3,
        .max_chans = 3,
        .rate_family_flags = ASF_RANGE_FLAG_FPS_48000_FAMILY,
    },
    {
        .flags = AUDIO_SAMPLE_FORMAT_16BIT,
        .min_rate = 8000,
        .max_rate = 8000,
        .min_chans = 1,
        .max_chans = 1,
        .rate_family_flags = ASF_RANGE_FLAG_FPS_CONTINUOUS,
    },
    {
        .flags = AUDIO_SAMPLE_FORMAT_16BIT,
        .min_rate = 48000,
        .max_rate = 48000,
        .min_chans = 1,
        .max_chans = 1,
        .rate_family_flags = ASF_RANGE_FLAG_FPS_48000_FAMILY,
    },
    {
        .flags = AUDIO_SAMPLE_FORMAT_16BIT,
        .min_rate = 96000,
        .max_rate = 96000,
        .min_chans = 2,
        .max_chans = 2,
        .rate_family_flags = ASF_RANGE_FLAG_FPS_CONTINUOUS,
    },
};

bool VirtualAudioUtil::AddFormatRange(const std::string& format_range_str) {
  uint8_t format_option =
      (format_range_str.empty() ? kDefaultFormatRangeOption
                                : fxl::StringToNumber<uint8_t>(format_range_str));
  if (format_option >= std::size(kFormatSpecs)) {
    printf("ERROR: Format range option must be %lu or less.\n", std::size(kFormatSpecs) - 1);
    return false;
  }
  fuchsia::virtualaudio::FormatRange range = {
      .sample_format_flags = kFormatSpecs[format_option].flags,
      .min_frame_rate = kFormatSpecs[format_option].min_rate,
      .max_frame_rate = kFormatSpecs[format_option].max_rate,
      .min_channels = kFormatSpecs[format_option].min_chans,
      .max_channels = kFormatSpecs[format_option].max_chans,
      .rate_family_flags = kFormatSpecs[format_option].rate_family_flags,
  };

  EnsureTypesExist();

  switch (config()->device_specific().Which()) {
    case fuchsia::virtualaudio::DeviceSpecific::Tag::kCodec:
      return false;  // Nothing to do here (no RingBuffer).
    case fuchsia::virtualaudio::DeviceSpecific::Tag::kComposite: {
      auto& composite = config()->mutable_device_specific()->composite();
      // Set formats for all ring buffers.
      for (auto& i : *composite.mutable_ring_buffers()) {
        i.mutable_ring_buffer()->mutable_supported_formats()->push_back(range);
      }
      return true;
    }
    case fuchsia::virtualaudio::DeviceSpecific::Tag::kDai: {
      auto& dai = config()->mutable_device_specific()->dai();
      dai.mutable_ring_buffer()->mutable_supported_formats()->push_back(range);
      return true;
    }
    case fuchsia::virtualaudio::DeviceSpecific::Tag::kStreamConfig: {
      auto& stream_config = config()->mutable_device_specific()->stream_config();
      stream_config.mutable_ring_buffer()->mutable_supported_formats()->push_back(range);
      return true;
    }
    default:
      return false;
  }
}

bool VirtualAudioUtil::ClearFormatRanges() {
  EnsureTypesExist();

  switch (config()->device_specific().Which()) {
    case fuchsia::virtualaudio::DeviceSpecific::Tag::kCodec:
      return false;  // Nothing to do here (no RingBuffer).
    case fuchsia::virtualaudio::DeviceSpecific::Tag::kComposite: {
      auto& composite = config()->mutable_device_specific()->composite();
      // Clear format ranges for all ring buffers.
      for (auto& i : *composite.mutable_ring_buffers()) {
        i.mutable_ring_buffer()->mutable_supported_formats()->clear();
      }
      return true;
    }
    case fuchsia::virtualaudio::DeviceSpecific::Tag::kDai: {
      config()
          ->mutable_device_specific()
          ->dai()
          .mutable_ring_buffer()
          ->mutable_supported_formats()
          ->clear();
      return true;
    }
    case fuchsia::virtualaudio::DeviceSpecific::Tag::kStreamConfig: {
      config()
          ->mutable_device_specific()
          ->stream_config()
          .mutable_ring_buffer()
          ->mutable_supported_formats()
          ->clear();
      return true;
    }
    default:
      return false;
  }
}

bool VirtualAudioUtil::SetTransferBytes(const std::string& transfer_bytes_str) {
  uint32_t driver_transfer_bytes = transfer_bytes_str.empty()
                                       ? kDefaultTransferBytes
                                       : fxl::StringToNumber<uint32_t>(transfer_bytes_str);

  EnsureTypesExist();

  switch (config()->device_specific().Which()) {
    case fuchsia::virtualaudio::DeviceSpecific::Tag::kCodec:
      return false;  // Nothing to do here (no RingBuffer).
    case fuchsia::virtualaudio::DeviceSpecific::Tag::kComposite: {
      auto& composite = config()->mutable_device_specific()->composite();
      // Set driver transfer bytes for all ring buffers.
      for (auto& i : *composite.mutable_ring_buffers()) {
        i.mutable_ring_buffer()->set_driver_transfer_bytes(driver_transfer_bytes);
      }
      return true;
    }
    case fuchsia::virtualaudio::DeviceSpecific::Tag::kDai: {
      auto& dai = config()->mutable_device_specific()->dai();
      dai.mutable_ring_buffer()->set_driver_transfer_bytes(driver_transfer_bytes);
      return true;
    }
    case fuchsia::virtualaudio::DeviceSpecific::Tag::kStreamConfig: {
      auto& stream_config = config()->mutable_device_specific()->stream_config();
      stream_config.mutable_ring_buffer()->set_driver_transfer_bytes(driver_transfer_bytes);
      return true;
    }
    default:
      return false;
  }
}

bool VirtualAudioUtil::SetInternalDelay(const std::string& delay_str) {
  zx_duration_t internal_delay =
      delay_str.empty() ? kDefaultInternalDelayNsec : fxl::StringToNumber<zx_duration_t>(delay_str);

  EnsureTypesExist();
  switch (config()->device_specific().Which()) {
    case fuchsia::virtualaudio::DeviceSpecific::Tag::kCodec:
      return false;  // Nothing to do here (no RingBuffer).
    case fuchsia::virtualaudio::DeviceSpecific::Tag::kComposite: {
      auto& composite = config()->mutable_device_specific()->composite();
      // For now, set internal delay for all ring buffers.
      for (auto& i : *composite.mutable_ring_buffers()) {
        i.mutable_ring_buffer()->set_internal_delay(internal_delay);
      }
      return true;
    }
    case fuchsia::virtualaudio::DeviceSpecific::Tag::kDai: {
      auto& dai = config()->mutable_device_specific()->dai();
      dai.mutable_ring_buffer()->set_internal_delay(internal_delay);
      return true;
    }
    case fuchsia::virtualaudio::DeviceSpecific::Tag::kStreamConfig: {
      auto& stream_config = config()->mutable_device_specific()->stream_config();
      stream_config.mutable_ring_buffer()->set_internal_delay(internal_delay);
      return true;
    }
    default:
      return false;
  }
}

bool VirtualAudioUtil::SetExternalDelay(const std::string& delay_str) {
  zx_duration_t external_delay =
      delay_str.empty() ? kDefaultExternalDelayNsec : fxl::StringToNumber<zx_duration_t>(delay_str);

  EnsureTypesExist();
  switch (config()->device_specific().Which()) {
    case fuchsia::virtualaudio::DeviceSpecific::Tag::kCodec:
      return false;  // Nothing to do here (no RingBuffer).
    case fuchsia::virtualaudio::DeviceSpecific::Tag::kComposite: {
      auto& composite = config()->mutable_device_specific()->composite();
      // Set external delay for all ring buffers.
      for (auto& i : *composite.mutable_ring_buffers()) {
        i.mutable_ring_buffer()->set_external_delay(external_delay);
      }
      return true;
    }
    case fuchsia::virtualaudio::DeviceSpecific::Tag::kDai: {
      auto& dai = config()->mutable_device_specific()->dai();
      dai.mutable_ring_buffer()->set_external_delay(external_delay);
      return true;
    }
    case fuchsia::virtualaudio::DeviceSpecific::Tag::kStreamConfig: {
      auto& stream_config = config()->mutable_device_specific()->stream_config();
      stream_config.mutable_ring_buffer()->set_external_delay(external_delay);
      return true;
    }
    default:
      return false;
  }
}

struct BufferSpec {
  uint32_t min_frames;
  uint32_t max_frames;
  uint32_t mod_frames;
};

// Buffer sizes (at default 48kHz rate): [0] 1.0-1.5 sec, in steps of 0.125;
// [1] 0.2-0.6 sec, in steps of 0.01;    [2] exactly 2 secs;    [3] exactly 6 secs.
constexpr BufferSpec kBufferSpecs[4] = {
    {.min_frames = 48000, .max_frames = 72000, .mod_frames = 6000},
    {.min_frames = 9600, .max_frames = 28800, .mod_frames = 480},
    {.min_frames = 96000, .max_frames = 96000, .mod_frames = 96000},
    {.min_frames = 288000, .max_frames = 288000, .mod_frames = 288000},
};

bool VirtualAudioUtil::SetRingBufferRestrictions(const std::string& rb_restr_str) {
  uint8_t rb_option = (rb_restr_str.empty() ? kDefaultRingBufferOption
                                            : fxl::StringToNumber<uint8_t>(rb_restr_str));
  if (rb_option >= std::size(kBufferSpecs)) {
    printf("ERROR: Ring buffer option must be %lu or less.\n", std::size(kBufferSpecs) - 1);
    return false;
  }

  fuchsia::virtualaudio::RingBufferConstraints ring_buffer_constraints;
  ring_buffer_constraints.min_frames = kBufferSpecs[rb_option].min_frames;
  ring_buffer_constraints.max_frames = kBufferSpecs[rb_option].max_frames;
  ring_buffer_constraints.modulo_frames = kBufferSpecs[rb_option].mod_frames;

  EnsureTypesExist();

  switch (config()->device_specific().Which()) {
    case fuchsia::virtualaudio::DeviceSpecific::Tag::kCodec:
      return false;  // Nothing to do here (no RingBuffer).
    case fuchsia::virtualaudio::DeviceSpecific::Tag::kComposite: {
      auto& composite = config()->mutable_device_specific()->composite();
      // Set ring buffer constrains for all ring buffers.
      for (auto& i : *composite.mutable_ring_buffers()) {
        i.mutable_ring_buffer()->set_ring_buffer_constraints(ring_buffer_constraints);
      }
      return true;
    }
    case fuchsia::virtualaudio::DeviceSpecific::Tag::kDai: {
      auto& dai = config()->mutable_device_specific()->dai();
      dai.mutable_ring_buffer()->set_ring_buffer_constraints(ring_buffer_constraints);
      return true;
    }
    case fuchsia::virtualaudio::DeviceSpecific::Tag::kStreamConfig: {
      auto& stream_config = config()->mutable_device_specific()->stream_config();
      stream_config.mutable_ring_buffer()->set_ring_buffer_constraints(ring_buffer_constraints);
      return true;
    }
    default:
      return false;
  }
}

struct GainSpec {
  bool muted;
  bool agc_enabled;
  float gain_db;
  bool can_mute;
  bool can_agc;
  float min_gain_db;
  float max_gain_db;
  float gain_step_db;
};

// The utility defines two preset groups of gain options. Although arbitrarily chosen, they exercise
// the available range through SetGainProperties:
// 0.Can and is mute.    Cannot AGC.       Gain -2,  range [-60, 0] in 2.0dB.
// 1.Can but isn't mute. Can AGC, enabled. Gain -7.5,range [-30,+2] in 0.5db.
// 2.Cannot mute.        Cannot AGC.       Gain 0,   range [0,0]    in 0db.
constexpr GainSpec kGainSpecs[] = {
    {
        .muted = true,
        .agc_enabled = false,
        .gain_db = -2.0,
        .can_mute = true,
        .can_agc = false,
        .min_gain_db = -60.0,
        .max_gain_db = 0.0,
        .gain_step_db = 2.0,
    },
    {
        .muted = false,
        .agc_enabled = true,
        .gain_db = -7.5,
        .can_mute = true,
        .can_agc = true,
        .min_gain_db = -30.0,
        .max_gain_db = 2.0,
        .gain_step_db = 0.5,
    },
    {
        .muted = false,
        .agc_enabled = false,
        .gain_db = 0.0,
        .can_mute = false,
        .can_agc = false,
        .min_gain_db = 0.0,
        .max_gain_db = 0.0,
        .gain_step_db = 0.0,
    },
};

bool VirtualAudioUtil::SetGainProps(const std::string& gain_props_str) {
  uint8_t gain_props_option =
      (gain_props_str.empty() ? kDefaultGainPropsOption
                              : fxl::StringToNumber<uint8_t>(gain_props_str));
  if (gain_props_option >= std::size(kGainSpecs)) {
    printf("ERROR: Gain properties option must be %lu or less.\n", std::size(kGainSpecs));
    return false;
  }

  fuchsia::virtualaudio::GainProperties props;
  props.set_min_gain_db(kGainSpecs[gain_props_option].min_gain_db);
  props.set_max_gain_db(kGainSpecs[gain_props_option].max_gain_db);
  props.set_gain_step_db(kGainSpecs[gain_props_option].gain_step_db);
  props.set_can_mute(kGainSpecs[gain_props_option].can_mute);
  props.set_can_agc(kGainSpecs[gain_props_option].can_agc);
  fuchsia::hardware::audio::GainState gain_state;
  gain_state.set_gain_db(kGainSpecs[gain_props_option].gain_db);
  gain_state.set_muted(kGainSpecs[gain_props_option].muted);
  gain_state.set_agc_enabled(kGainSpecs[gain_props_option].agc_enabled);
  props.set_gain_state(std::move(gain_state));

  EnsureTypesExist();

  switch (config()->device_specific().Which()) {
    case fuchsia::virtualaudio::DeviceSpecific::Tag::kStreamConfig: {
      auto& stream_config = config()->mutable_device_specific()->stream_config();
      *stream_config.mutable_gain_properties() = std::move(props);
      return true;
    }
    default:
      return false;
  }
}

// These preset options represent the following common configurations:
// 0.(Default) Hot-pluggable;   1.Hardwired;    2.Hot-pluggable, unplugged;
// 3.Plugged (synch: detected only by polling); 4.Unplugged (synch)
constexpr audio_pd_notify_flags_t kPlugFlags[] = {
    AUDIO_PDNF_PLUGGED /*AUDIO_PDNF_HARDWIRED*/ | AUDIO_PDNF_CAN_NOTIFY,
    AUDIO_PDNF_PLUGGED | AUDIO_PDNF_HARDWIRED /*  AUDIO_PDNF_CAN_NOTIFY*/,
    /*AUDIO_PDNF_PLUGGED AUDIO_PDNF_HARDWIRED  */ AUDIO_PDNF_CAN_NOTIFY,
    AUDIO_PDNF_PLUGGED /*AUDIO_PDNF_HARDWIRED     AUDIO_PDNF_CAN_NOTIFY*/,
    0 /*AUDIO_PDNF_PLUGGED AUDIO_PDNF_HARDWIRED   AUDIO_PDNF_CAN_NOTIFY*/
};

constexpr zx_time_t kPlugTime[] = {0, -1, -1, ZX_SEC(1), ZX_SEC(2)};
static_assert(std::size(kPlugFlags) == std::size(kPlugTime));

bool VirtualAudioUtil::SetPlugProps(const std::string& plug_props_str) {
  uint8_t plug_props_option =
      (plug_props_str.empty() ? kDefaultPlugPropsOption
                              : fxl::StringToNumber<uint8_t>(plug_props_str));

  if (plug_props_option >= std::size(kPlugFlags)) {
    printf("ERROR: Plug properties option must be %lu or less.\n", std::size(kPlugFlags) - 1);
    return false;
  }

  fuchsia::virtualaudio::PlugProperties props;
  fuchsia::hardware::audio::PlugState plug_state;
  plug_state.set_plugged(kPlugFlags[plug_props_option] & AUDIO_PDNF_PLUGGED);
  plug_state.set_plug_state_time(kPlugTime[plug_props_option]);
  props.set_plug_state(std::move(plug_state));
  if (kPlugFlags[plug_props_option] & AUDIO_PDNF_HARDWIRED) {
    props.set_plug_detect_capabilities(fuchsia::hardware::audio::PlugDetectCapabilities::HARDWIRED);
  } else if (kPlugFlags[plug_props_option] & AUDIO_PDNF_CAN_NOTIFY) {
    props.set_plug_detect_capabilities(
        fuchsia::hardware::audio::PlugDetectCapabilities::CAN_ASYNC_NOTIFY);
  }

  EnsureTypesExist();

  switch (config()->device_specific().Which()) {
    case fuchsia::virtualaudio::DeviceSpecific::Tag::kStreamConfig: {
      auto& stream_config = config()->mutable_device_specific()->stream_config();
      *stream_config.mutable_plug_properties() = std::move(props);
      return true;
    }
    case fuchsia::virtualaudio::DeviceSpecific::Tag::kCodec: {
      auto& codec = config()->mutable_device_specific()->codec();
      *codec.mutable_plug_properties() = std::move(props);
      return true;
    }
    default:
      return false;
  }
}

bool VirtualAudioUtil::AdjustClockRate(const std::string& clock_adjust_str) {
  int32_t clock_domain = 0;

  auto rate_adjustment_ppm = fxl::StringToNumber<int32_t>(clock_adjust_str);
  if (rate_adjustment_ppm < ZX_CLOCK_UPDATE_MIN_RATE_ADJUST ||
      rate_adjustment_ppm > ZX_CLOCK_UPDATE_MAX_RATE_ADJUST) {
    printf("ERROR: Clock rate adjustment must be within [%d, %d].\n",
           ZX_CLOCK_UPDATE_MIN_RATE_ADJUST, ZX_CLOCK_UPDATE_MAX_RATE_ADJUST);
    return false;
  }

  EnsureTypesExist();

  switch (config()->device_specific().Which()) {
    case fuchsia::virtualaudio::DeviceSpecific::Tag::kCodec:
      return false;  // Nothing to do here.
    case fuchsia::virtualaudio::DeviceSpecific::Tag::kComposite: {
      auto& composite = config()->mutable_device_specific()->composite();
      if (composite.has_clock_properties() && composite.clock_properties().has_domain()) {
        clock_domain = composite.clock_properties().domain();
      }
      break;
    }
    case fuchsia::virtualaudio::DeviceSpecific::Tag::kDai: {
      auto& dai = config()->mutable_device_specific()->dai();
      if (dai.has_clock_properties() && dai.clock_properties().has_domain()) {
        clock_domain = dai.clock_properties().domain();
      }
      break;
    }
    case fuchsia::virtualaudio::DeviceSpecific::Tag::kStreamConfig: {
      auto& stream_config = config()->mutable_device_specific()->stream_config();
      if (stream_config.has_clock_properties() && stream_config.clock_properties().has_domain()) {
        clock_domain = stream_config.clock_properties().domain();
      }
      break;
    }
    default:
      return false;
  }

  if (clock_domain == 0 && rate_adjustment_ppm != 0) {
    printf("WARNING: by definition, a clock in domain 0 should never have rate variance!\n");
  }
  (*device())->AdjustClockRate(
      rate_adjustment_ppm,
      [](fuchsia::virtualaudio::Device_AdjustClockRate_Result result) { CallbackReceived(); });
  return WaitForCallback();
}

bool VirtualAudioUtil::SetDirection(std::optional<bool> is_input) {
  configuring_input_ = is_input;
  switch (device_type_) {
    case fuchsia::virtualaudio::DeviceType::CODEC:
      // `is_input` is optional for a codec device.
      if (is_input.has_value()) {
        config()->mutable_device_specific()->codec().set_is_input(*is_input);
      } else {
        config()->mutable_device_specific()->codec().clear_is_input();
      }
      return true;
    case fuchsia::virtualaudio::DeviceType::COMPOSITE:
      // Can't set a direction on a composite device.
      return false;
    case fuchsia::virtualaudio::DeviceType::DAI:
      // Note: although `is_input` is a required DaiProperties field, a badly-behaved driver might
      // still fail to set it. That said, this incorrect behavior isn't possible in VAD yet.
      // So (for now at least) it is required for all virtual_audio DAI instances.
      if (is_input.has_value()) {
        config()->mutable_device_specific()->dai().set_is_input(*is_input);
        return true;
      }
      return false;
    case fuchsia::virtualaudio::DeviceType::STREAM_CONFIG:
      // Note: `is_input` is a required StreamProperties field, however a badly-behaved driver might
      // still fail to set it. That said, this incorrect behavior isn't possible in VAD yet: it uses
      // this bool when registering the stream_config in devfs (`audio-input` vs. `audio-output`).
      // So (for now at least) it is required for all virtual_audio StreamConfig instances.
      if (is_input.has_value()) {
        config()->mutable_device_specific()->stream_config().set_is_input(*is_input);
        return true;
      }
      return false;
    default:
      printf("ERROR: Unknown device type\n");
      return false;
  }
}

zx_status_t VirtualAudioUtil::ResetConfiguration(fuchsia::virtualaudio::DeviceType device_type,
                                                 std::optional<bool> is_input) {
  zx_status_t status = ZX_OK;
  fuchsia::virtualaudio::Direction direction;
  if (is_input) {
    direction.set_is_input(*is_input);
  }
  fuchsia::virtualaudio::Control_GetDefaultConfiguration_Result config_result;
  auto& controller = GetController(device_type);
  status = controller->GetDefaultConfiguration(device_type, std::move(direction), &config_result);
  if (status != ZX_OK) {
    printf("ERROR: Failed to send GetDefaultConfiguration request: %s\n",
           zx_status_get_string(status));
    return status;
  }
  if (config_result.is_err()) {
    status = static_cast<zx_status_t>(config_result.err());
    printf("ERROR: Failed to get default config for device, error = %d\n", status);
    return status;
  }

  *ConfigForDevice(is_input, device_type) = std::move(config_result.response().config);
  return ZX_OK;
}

bool VirtualAudioUtil::AddDevice() {
  fuchsia::virtualaudio::Configuration cfg;
  zx_status_t status = config()->Clone(&cfg);
  FX_CHECK(status == ZX_OK);

  auto request = (*device()).NewRequest();

  fuchsia::virtualaudio::Control_AddDevice_Result result;
  auto& controller = GetController(cfg.device_specific().Which());
  status = controller->AddDevice(std::move(cfg), std::move(request), &result);
  if (status == ZX_OK) {
    if (result.is_err()) {
      status = static_cast<zx_status_t>(result.err());
      printf("ERROR: AddDevice returned ZX_OK but result.err was %d\n", status);
    }
  }

  if (status != ZX_OK) {
    printf("ERROR: Failed to add %s device, status = %d\n",
           to_string(configuring_input_, "input", "output", "directionless").c_str(), status);
    QuitLoop();
    return false;
  }

  device()->set_error_handler([is_input = configuring_input_](zx_status_t error) {
    std::string direction;
    if (is_input) {
      direction = (*is_input ? "input" : "output");
    } else {
      direction = "directionless";
    }
    printf("%s device disconnected (%d)!\n", direction.c_str(), error);
    QuitLoop();
  });

  SetUpEvents();

  // let VirtualAudio disconnect if all is not well.
  bool success = (WaitForNoCallback() && device()->is_bound());

  if (!success) {
    printf("ERROR: Failed to establish channel to %s device\n",
           configuring_input_ ? "input" : "output");
  }
  return success;
}

bool VirtualAudioUtil::RemoveDevice() {
  device()->Unbind();
  return WaitForNoCallback();
}

bool VirtualAudioUtil::ChangePlugState(const std::string& plug_time_str, bool plugged) {
  if (!device()->is_bound()) {
    printf("ERROR: Device not bound - you must add the device before using this flag.\n");
    return false;
  }

  auto plug_change_time = (plug_time_str.empty() ? zx::clock::get_monotonic().get()
                                                 : fxl::StringToNumber<zx_time_t>(plug_time_str));

  (*device())->ChangePlugState(
      plug_change_time, plugged,
      [](fuchsia::virtualaudio::Device_ChangePlugState_Result result) { CallbackReceived(); });
  return WaitForCallback();
}

bool VirtualAudioUtil::GetFormat() {
  if (!device()->is_bound()) {
    printf("ERROR: Device not bound - you must add the device before using this flag.\n");
    return false;
  }

  if (configuring_input_) {
    (*device())->GetFormat(FormatCallback<false>);
  } else {
    (*device())->GetFormat(FormatCallback<true>);
  }

  return WaitForCallback();
}

bool VirtualAudioUtil::GetGain() {
  if (!device()->is_bound()) {
    printf("ERROR: Device not bound - you must add the device before using this flag.\n");
    return false;
  }

  if (configuring_input_) {
    (*device())->GetGain(GainCallback<false>);
  } else {
    (*device())->GetGain(GainCallback<true>);
  }

  return WaitForCallback();
}

bool VirtualAudioUtil::GetBuffer() {
  if (!device()->is_bound()) {
    printf("ERROR: Device not bound - you must add the device before using this flag.\n");
    return false;
  }

  if (configuring_input_) {
    (*device())->GetBuffer(BufferCallback<false>);
  } else {
    (*device())->GetBuffer(BufferCallback<true>);
  }

  return WaitForCallback() && ring_buffer_vmo_.is_valid();
}

bool VirtualAudioUtil::WriteBuffer(const std::string& write_value_str) {
  size_t value_to_write =
      (write_value_str.empty() ? kDefaultValueToWrite
                               : fxl::StringToNumber<size_t>(write_value_str, fxl::Base::k16));

  if (!ring_buffer_vmo_.is_valid()) {
    if (!GetBuffer()) {
      printf("ERROR: Failed to retrieve RingBuffer for writing.\n");
      return false;
    }
  }

  auto rb_size = rb_size_[configuring_input_ ? kInput : kOutput];
  for (size_t offset = 0; offset < rb_size; offset += sizeof(value_to_write)) {
    zx_status_t status = ring_buffer_vmo_.write(&value_to_write, offset, sizeof(value_to_write));
    if (status != ZX_OK) {
      printf("ERROR: Writing %16ld (0x%016zX) to rb_vmo[%zu] failed (%d)\n", value_to_write,
             value_to_write, offset, status);
      return false;
    }
  }

  printf("--Wrote %16ld (0x%016zX) across the ring buffer\n", value_to_write, value_to_write);

  return WaitForNoCallback();
}

bool VirtualAudioUtil::GetPosition() {
  if (!device()->is_bound()) {
    printf("ERROR: Device not bound - you must add the device before using this flag.\n");
    return false;
  }

  if (configuring_input_) {
    (*device())->GetPosition(PositionCallback<false>);
  } else {
    (*device())->GetPosition(PositionCallback<true>);
  }

  return WaitForCallback();
}

bool VirtualAudioUtil::SetNotificationFrequency(const std::string& notifs_str) {
  if (!device()->is_bound()) {
    printf("ERROR: Device not bound - you must add the device before using this flag.\n");
    return false;
  }

  uint32_t notifications_per_ring =
      (notifs_str.empty() ? kDefaultNotificationFrequency
                          : fxl::StringToNumber<uint32_t>(notifs_str));
  (*device())->SetNotificationFrequency(
      notifications_per_ring,
      [](fuchsia::virtualaudio::Device_SetNotificationFrequency_Result result) {
        CallbackReceived();
      });

  return WaitForCallback();
}

void VirtualAudioUtil::CallbackReceived() {
  VirtualAudioUtil::received_callback_ = true;
  VirtualAudioUtil::loop_->Quit();
}

template <bool is_out>
void VirtualAudioUtil::FormatNotification(uint32_t fps, uint32_t fmt, uint32_t chans,
                                          zx_duration_t delay) {
  printf("--Received Format (%u fps, %x fmt, %u chan, %zu delay) for %s\n", fps, fmt, chans, delay,
         (is_out ? "output" : "input"));

  DeviceDirection dev_type = is_out ? kOutput : kInput;
  frame_size_[dev_type] = chans * BytesPerSample(fmt);
  ref_time_to_running_position_rate_[dev_type] =
      media::TimelineRate(fps * frame_size_[dev_type], ZX_SEC(1));
}
template <bool is_out>
void VirtualAudioUtil::FormatCallback(fuchsia::virtualaudio::Device_GetFormat_Result result) {
  VirtualAudioUtil::CallbackReceived();
  if (!result.is_response()) {
    printf("GetFormatfailed with error %d\n", static_cast<int32_t>(result.err()));
    return;
  }
  VirtualAudioUtil::FormatNotification<is_out>(
      result.response().frames_per_second, result.response().sample_format,
      result.response().num_channels, result.response().external_delay);
}

template <bool is_out>
void VirtualAudioUtil::GainNotification(bool mute, bool agc, float gain_db) {
  printf("--Received Gain   (mute: %u, agc: %u, gain: %.5f dB) for %s\n", mute, agc, gain_db,
         (is_out ? "output" : "input"));
}
template <bool is_out>
void VirtualAudioUtil::GainCallback(fuchsia::virtualaudio::Device_GetGain_Result result) {
  VirtualAudioUtil::CallbackReceived();
  if (result.is_err()) {
    printf("ERROR: Get gain failed\n");
    return;
  }
  VirtualAudioUtil::GainNotification<is_out>(result.response().current_mute,
                                             result.response().current_agc,
                                             result.response().current_gain_db);
}

template <bool is_out>
void VirtualAudioUtil::BufferNotification(zx::vmo ring_buffer_vmo, uint32_t num_ring_buffer_frames,
                                          uint32_t notifications_per_ring) {
  ring_buffer_vmo_ = std::move(ring_buffer_vmo);
  uint64_t vmo_size;
  ring_buffer_vmo_.get_size(&vmo_size);
  DeviceDirection dev_type = is_out ? kOutput : kInput;
  rb_size_[dev_type] = (static_cast<size_t>(num_ring_buffer_frames * frame_size_[dev_type]));

  printf("--Received SetBuffer (vmo size: %zu, ring size: %zu, frames: %u, notifs: %u) for %s\n",
         vmo_size, rb_size_[dev_type], num_ring_buffer_frames, notifications_per_ring,
         (is_out ? "output" : "input"));
}
template <bool is_out>
void VirtualAudioUtil::BufferCallback(fuchsia::virtualaudio::Device_GetBuffer_Result result) {
  VirtualAudioUtil::CallbackReceived();
  if (!result.is_response()) {
    printf("GetBuffer failed with error %d\n", static_cast<int32_t>(result.err()));
    return;
  }
  VirtualAudioUtil::BufferNotification<is_out>(std::move(result.response().ring_buffer),
                                               result.response().num_ring_buffer_frames,
                                               result.response().notifications_per_ring);
}

void VirtualAudioUtil::UpdateRunningPosition(uint32_t ring_position, bool is_output) {
  auto dev_type = is_output ? kOutput : kInput;

  if (ring_position <= last_rb_position_[dev_type]) {
    running_position_[dev_type] += rb_size_[dev_type];
  }
  running_position_[dev_type] -= last_rb_position_[dev_type];
  running_position_[dev_type] += ring_position;
  last_rb_position_[dev_type] = ring_position;
}

template <bool is_out>
void VirtualAudioUtil::StartNotification(zx_time_t start_time) {
  printf("--Received Start    (time: %zu) for %s\n", start_time, (is_out ? "output" : "input"));

  DeviceDirection dev_type = is_out ? kOutput : kInput;
  ref_time_to_running_position_[dev_type] =
      media::TimelineFunction(0, start_time, ref_time_to_running_position_rate_[dev_type]);

  running_position_[dev_type] = 0;
  last_rb_position_[dev_type] = 0;
}

template <bool is_out>
void VirtualAudioUtil::StopNotification(zx_time_t stop_time, uint32_t ring_position) {
  DeviceDirection dev_type = is_out ? kOutput : kInput;
  auto expected_running_position = ref_time_to_running_position_[dev_type].Apply(stop_time);
  UpdateRunningPosition(ring_position, is_out);

  printf("--Received Stop     (time: %zu, pos: %u) for %s\n", stop_time, ring_position,
         (is_out ? "output" : "input"));
  printf("--Stop at  position: expected %zu; actual %zu\n", expected_running_position,
         running_position_[dev_type]);

  running_position_[dev_type] = 0;
  last_rb_position_[dev_type] = 0;
}

template <bool is_out>
void VirtualAudioUtil::PositionNotification(zx_time_t monotonic_time_for_position,
                                            uint32_t ring_position) {
  printf("--Received Position (time: %13zu, pos: %6u) for %s", monotonic_time_for_position,
         ring_position, (is_out ? "output" : "input "));

  DeviceDirection dev_type = is_out ? kOutput : kInput;
  if (monotonic_time_for_position > ref_time_to_running_position_[dev_type].reference_time()) {
    int64_t expected_running_position =
        ref_time_to_running_position_[dev_type].Apply(monotonic_time_for_position);

    UpdateRunningPosition(ring_position, is_out);
    FX_CHECK(running_position_[dev_type] <= std::numeric_limits<int64_t>::max());
    int64_t delta = expected_running_position - static_cast<int64_t>(running_position_[dev_type]);
    printf(" - running byte position: expect %8zu  actual %8zu  delta %6zd",
           expected_running_position, running_position_[dev_type], delta);
  }
  printf("\n");
}
template <bool is_out>
void VirtualAudioUtil::PositionCallback(fuchsia::virtualaudio::Device_GetPosition_Result result) {
  VirtualAudioUtil::CallbackReceived();
  if (!result.is_response()) {
    printf("GetPosition failed with error %d\n", static_cast<int32_t>(result.err()));
    return;
  }
  VirtualAudioUtil::PositionNotification<is_out>(result.response().monotonic_time,
                                                 result.response().ring_position);
}

// Convenience method that allows us to set configuration without having to check that some FIDL
// table members have been defined.
void VirtualAudioUtil::EnsureTypesExist() {
  switch (config()->device_specific().Which()) {
    case fuchsia::virtualaudio::DeviceSpecific::Tag::kCodec:
      break;  // Nothing for Codec to set up ahead of time (no RingBuffer).
    case fuchsia::virtualaudio::DeviceSpecific::Tag::kComposite: {
      auto& composite = config()->mutable_device_specific()->composite();
      // Not all composite drivers have a ring buffer, this convenience code sets a composite
      // driver's first ring buffer related FIDL table members, so user of this method do not
      // have to check for their existence.
      if (!composite.has_ring_buffers()) {
        composite.set_ring_buffers(std::vector<fuchsia::virtualaudio::CompositeRingBuffer>(1));
      }
      for (auto& i : *composite.mutable_ring_buffers()) {
        if (!i.has_ring_buffer()) {
          i.set_ring_buffer(fuchsia::virtualaudio::RingBuffer{});
        }
      }
      break;
    }
    case fuchsia::virtualaudio::DeviceSpecific::Tag::kDai: {
      auto& dai = config()->mutable_device_specific()->dai();
      if (!dai.has_ring_buffer()) {
        dai.set_ring_buffer(fuchsia::virtualaudio::RingBuffer{});
      }
      break;
    }
    case fuchsia::virtualaudio::DeviceSpecific::Tag::kStreamConfig: {
      auto& stream_config = config()->mutable_device_specific()->stream_config();
      if (!stream_config.has_ring_buffer()) {
        stream_config.set_ring_buffer(fuchsia::virtualaudio::RingBuffer{});
      }
      break;
    }
    default:
      break;
  }
}

fuchsia::virtualaudio::ControlSyncPtr& VirtualAudioUtil::GetController(
    fuchsia::virtualaudio::DeviceType device_type) {
  switch (device_type) {
    case fuchsia::virtualaudio::DeviceType::CODEC:
    case fuchsia::virtualaudio::DeviceType::DAI:
    case fuchsia::virtualaudio::DeviceType::STREAM_CONFIG:
      return legacy_controller_;
    case fuchsia::virtualaudio::DeviceType::COMPOSITE:
      return controller_;
    default:
      printf("ERROR: unknown DeviceType\n");
      return controller_;
  }
}

fuchsia::virtualaudio::ControlSyncPtr& VirtualAudioUtil::GetController(
    fuchsia::virtualaudio::DeviceSpecific::Tag device_type) {
  switch (device_type) {
    case fuchsia::virtualaudio::DeviceSpecific::Tag::kCodec:
    case fuchsia::virtualaudio::DeviceSpecific::Tag::kDai:
    case fuchsia::virtualaudio::DeviceSpecific::Tag::kStreamConfig:
      return legacy_controller_;
    case fuchsia::virtualaudio::DeviceSpecific::Tag::kComposite:
      return controller_;
    default:
      printf("ERROR: unknown DeviceSpecific::Tag\n");
      return controller_;
  }
}

}  // namespace virtual_audio

int main(int argc, const char** argv) {
  fuchsia_logging::LogSettingsBuilder builder;
  builder.WithTags({"virtual_audio_util"}).BuildAndInitialize();

  fxl::CommandLine command_line = fxl::CommandLineFromArgcArgv(argc, argv);
  async::Loop loop(&kAsyncLoopConfigAttachToCurrentThread);

  virtual_audio::VirtualAudioUtil util(&loop);
  util.Run(&command_line);

  return 0;
}
