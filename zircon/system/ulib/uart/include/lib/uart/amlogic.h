// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef LIB_UART_AMLOGIC_H_
#define LIB_UART_AMLOGIC_H_

#include <lib/stdcompat/array.h>
#include <lib/zbi-format/driver-config.h>
#include <lib/zbi-format/zbi.h>

#include <algorithm>

#include <hwreg/bitfields.h>

#include "interrupt.h"
#include "uart.h"

namespace uart::amlogic {

struct FifoRegister : public hwreg::RegisterBase<FifoRegister, uint32_t> {
  DEF_RSVDZ_FIELD(31, 8);
  DEF_FIELD(7, 0, data);

  static auto Get(uint32_t offset) { return hwreg::RegisterAddr<FifoRegister>(offset); }
};

struct WriteFifoRegister {
  static auto Get() { return FifoRegister::Get(0x0); }
};

struct ReadFifoRegister {
  static auto Get() { return FifoRegister::Get(0x4); }
};

struct ControlRegister : public hwreg::RegisterBase<ControlRegister, uint32_t> {
  enum class Bits {
    k8 = 0b00,
    k7 = 0b01,
    k6 = 0b10,
    k5 = 0b11,
  };

  enum class StopBits {
    k1 = 0b00,
    k2 = 0b01,
  };

  DEF_BIT(31, invert_rts);
  DEF_BIT(30, mask_error);
  DEF_BIT(29, invert_cts);
  DEF_BIT(28, tx_interrupt);
  DEF_BIT(27, rx_interrupt);
  DEF_BIT(26, invert_tx);
  DEF_BIT(25, invert_rx);
  DEF_BIT(24, clear_error);
  DEF_BIT(23, rx_reset);
  DEF_BIT(22, tx_reset);
  DEF_ENUM_FIELD(Bits, 21, 20, bits);
  DEF_BIT(19, parity_enable);
  DEF_BIT(18, parity_odd);
  DEF_ENUM_FIELD(StopBits, 17, 16, stop_bits);
  DEF_BIT(15, two_wire);
  // Bit 14 is unused.
  DEF_BIT(13, rx_enable);
  DEF_BIT(12, tx_enable);
  DEF_FIELD(11, 0, old_baud_rate);

  static auto Get() { return hwreg::RegisterAddr<ControlRegister>(0x8); }
};

struct StatusRegister : public hwreg::RegisterBase<StatusRegister, uint32_t> {
  // Bits [31:27] are unused.
  DEF_BIT(26, rx_busy);
  DEF_BIT(25, tx_busy);
  DEF_BIT(24, rx_fifo_overflow);
  DEF_BIT(23, cts);
  DEF_BIT(22, tx_fifo_empty);
  DEF_BIT(21, tx_fifo_full);
  DEF_BIT(20, rx_fifo_empty);
  DEF_BIT(19, rx_fifo_full);
  DEF_BIT(18, fifo_written_when_full);
  DEF_BIT(17, frame_error);
  DEF_BIT(16, parity_error);
  // Bit 15 is unused.
  DEF_FIELD(14, 8, tx_fifo_count);
  // Bit 7 is unused.
  DEF_FIELD(6, 0, rx_fifo_count);

  static auto Get() { return hwreg::RegisterAddr<StatusRegister>(0xc); }
};

struct IrqControlRegister : public hwreg::RegisterBase<IrqControlRegister, uint32_t> {
  DEF_FIELD(15, 8, tx_irq_count);
  DEF_FIELD(7, 0, rx_irq_count);

  static auto Get() { return hwreg::RegisterAddr<IrqControlRegister>(0x10); }
};

// The number of `IoSlots` used by this driver, determined by the last accessed register, see
// `IrqControlRegister`. For unscaled MMIO, this corresponds to the size of the MMIO region
// from a provided base address.
static constexpr size_t kIoSlots = 0x10 + sizeof(uint32_t);

struct Driver : public DriverBase<Driver, ZBI_KERNEL_DRIVER_AMLOGIC_UART, zbi_dcfg_simple_t,
                                  IoRegisterType::kMmio8, kIoSlots> {
  static constexpr auto kDevicetreeBindings =
      cpp20::to_array<std::string_view>({"amlogic,meson-gx-uart", "amlogic,meson-ao-uart"});

  static constexpr std::string_view kConfigName = "amlogic";
  static constexpr uint32_t kFifoDepth = 64;

  template <typename... Args>
  explicit Driver(Args&&... args)
      : DriverBase<Driver, ZBI_KERNEL_DRIVER_AMLOGIC_UART, zbi_dcfg_simple_t,
                   IoRegisterType::kMmio8, kIoSlots>(std::forward<Args>(args)...) {}

  template <class IoProvider>
  void Init(IoProvider& io) {
    // The line control settings were initialized by the hardware or the boot
    // loader and we just use them as they are.
    ControlRegister::Get()
        .ReadFrom(io.io())
        .set_rx_reset(true)
        .set_tx_reset(true)
        .set_clear_error(true)
        .set_tx_enable(true)
        .set_rx_enable(true)
        .set_tx_interrupt(false)
        .set_rx_interrupt(false)
        .set_two_wire(true)
        .WriteTo(io.io())
        // Must change state of rx/tx reset back to non reset or IRQs might
        // not work properly.
        .set_clear_error(false)
        .set_rx_reset(false)
        .set_tx_reset(false)
        .WriteTo(io.io());
  }

  template <class IoProvider>
  uint32_t TxReady(IoProvider& io) {
    auto sr = StatusRegister::Get().ReadFrom(io.io());
    if (sr.tx_fifo_full()) {
      return 0;
    }
    // Be careful about the assumed maximum it will report.
    return kFifoDepth - std::min(sr.tx_fifo_count(), kFifoDepth);
  }

  template <class IoProvider, typename It1, typename It2>
  auto Write(IoProvider& io, uint32_t ready_space, It1 it, const It2& end) {
    auto tx = WriteFifoRegister::Get().FromValue(0);
    do {
      tx.set_data(*it).WriteTo(io.io());
    } while (++it != end && --ready_space > 0);
    return it;
  }

  template <class IoProvider>
  std::optional<uint8_t> Read(IoProvider& io) {
    if (StatusRegister::Get().ReadFrom(io.io()).rx_fifo_empty()) {
      return {};
    }
    return ReadFifoRegister::Get().ReadFrom(io.io()).data();
  }

  template <class IoProvider>
  void EnableTxInterrupt(IoProvider& io, bool enable = true) {
    auto cr = ControlRegister::Get().ReadFrom(io.io());
    cr.set_tx_interrupt(enable).WriteTo(io.io());
  }

  template <class IoProvider>
  void EnableRxInterrupt(IoProvider& io, bool enable = true) {
    auto cr = ControlRegister::Get().ReadFrom(io.io());
    cr.set_rx_interrupt(enable).WriteTo(io.io());
  }

  template <class IoProvider, typename EnableInterruptCallback>
  void InitInterrupt(IoProvider& io, EnableInterruptCallback&& enable_interrupt_callback) {
    auto icr = IrqControlRegister::Get().ReadFrom(io.io());
    icr.set_tx_irq_count(kFifoDepth / 8).set_rx_irq_count(1).WriteTo(io.io());

    // Enable receive interrupts.
    // Transmit interrupts are enabled only when there is a blocked writer.
    EnableRxInterrupt(io);
    enable_interrupt_callback();
  }

  template <class IoProvider, typename Lock, typename Waiter, typename Tx, typename Rx>
  void Interrupt(IoProvider& io, Lock& lock, Waiter& waiter, Tx&& tx, Rx&& rx) {
    size_t drained_rx = 0;
    while (drained_rx < kFifoDepth) {
      auto sr = StatusRegister::Get().ReadFrom(io.io());
      auto cr = ControlRegister::Get().ReadFrom(io.io());

      // Drain characters in the fifo.
      bool rx_disabled = false;
      // Drain at most |kFifoDepth| characters per IRQ.
      // If there were no characters in the fifo, then it was either an error
      // or TX IRQ, will be handled in this pass.
      drained_rx += sr.rx_fifo_count() == 0 ? kFifoDepth : sr.rx_fifo_count();
      auto rx_irq = RxInterrupt(
          lock,  //
          [&]() { return ReadFifoRegister::Get().ReadFrom(io.io()).data(); },
          [&]() {
            // If the buffer is full, disable the receive interrupt instead
            // and stop checking.
            EnableRxInterrupt(io, false);
            rx_disabled = true;
          });
      for (size_t i = 0; i < sr.rx_fifo_count() && !rx_disabled; ++i) {
        rx(rx_irq);
      }

      // Clear any interrupt due to errors.
      if (sr.frame_error() || sr.parity_error()) {
        cr.set_clear_error(true).WriteTo(io.io()).set_clear_error(false).WriteTo(io.io());
      }

      if (cr.tx_interrupt() && !sr.tx_fifo_full()) {
        auto tx_irq = TxInterrupt(lock, waiter, [&]() { EnableTxInterrupt(io, false); });
        tx(tx_irq);
      }
    }
  }
};

}  // namespace uart::amlogic

#endif  // LIB_UART_AMLOGIC_H_
