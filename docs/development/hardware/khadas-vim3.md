# Khadas VIM3 development guide

The [Khadas VIM3] is an ARM64-based single board computer. It is possible to run
Fuchsia on the VIM3. This guide shows Fuchsia [contributors] how to [install
Fuchsia on a VIM3] and do other common development tasks.

See [Appendix: Feature support](#features) for details on which VIM3 features
Fuchsia supports.

## Audience {#audience}

This guide assumes you're comfortable with:

* Tinkering with electronics and hardware.
* Building Fuchsia from source and other CLI workflows.

## Install Fuchsia on a Khadas VIM3 board {#install}

See [Troubleshooting](#troubleshooting) and [Appendix: Support](#support) if you
have any trouble completing the installation process.

### Prerequisites {#prerequisites}

You'll need all of the following hardware and software:

* A Khadas VIM3 single-board computer. Googlers should request a board through
  the Fuchsia Ops team.

  Caution: Fuchsia is primarily focused on supporting the Pro model VIM3. The
  Basic model may work but is less of a priority.

* A desktop or laptop computer that's running Linux and has 2 USB ports
  available. This computer is called the **host**.

* A power supply of at least 24W to your host. The VIM3 can draw that much power
  when [DVFS] is enabled.

* A working Fuchsia development environment on your host. In other words, you
  should be able to [build Fuchsia] from its source code on your host.

* A [USB to TTL serial cable].

* A USB-C to USB-A cable that supports both data and power delivery. The USB-C
  side is for the VIM3. The other side is for your host.

  Caution: Other USB cable types, like USB-C to USB-C, may work, but appear to
  have power delivery issues more frequently than USB-C to USB-A. See
  [Troubleshooting: Bootloops].

The following is optional:

* A [heatsink]. This enables running 2 CPU cores on the VIM3 at full speed
  without reaching 80°C, the critical temperature beyond which cores are
  throttled down.

See the [VIM3 collection] in the Khadas shop for examples of compatible
accessories.

Note: All the links in this section are only for your convenience. You don't
need to buy from these exact stores or these exact parts.

### Build Fuchsia {#build}

If you don't already have an [in-tree][glossary.in-tree] environment set up,
you should start the process now because it can take a while to complete:

1. [Download the Fuchsia source code].

1. [Configure and build Fuchsia].

   * When configuring the build, use `fx set core.vim3`.

Note: The rest of this guide assumes that your Fuchsia source code directory is
located at `~/fuchsia`. This guide assumes that you run all `fx` commands from
`~/fuchsia`.

You'll use the Fuchsia development environment to build the Fuchsia image for
VIM3 and run an in-tree CLI tool for flashing the Fuchsia image onto the VIM3.

### Set up the hardware {#hardware}

Set up the VIM3 to communicate with your host:

1. Connect the VIM3 and your host to each other with the USB-C to USB-\* cable.
   The white LED on the VIM3 should turn on.

   Caution: Don't put a [USB hub] between the VIM3 and your host. The hub may
   make it harder for your VIM3 and host to detect and communicate with each
   other.

   This connection is used to power and flash the VIM3 with [`fastboot`].

1. Connect the serial cable wires to the VIM3's GPIOs:

   * GND to pin 17.

   * TX (out from VIM3) to pin 18.

   * RX (into VIM3) to pin 19.

   * Don't connect the power wire of your serial cable to any VIM3 GPIO.
     The VIM3 is getting power through the USB cable.

   Tip: Pins 1, 20, 21, and 40 are labeled on the circuit board.

   Caution: In general the colors for TX and RX wires are not standardized.
   For example your RX wire may be blue or green. If your host is unable to see
   the serial logs, power off the VIM3 board and swap the TX and RX connections.
   Some USB-to-serial adapters have their wires improperly labeled.

   See [Serial Debugging Tool] for an example image of how your serial wires
   should be connected to the VIM3.

1. Connect the USB end of the serial cable to your host.

#### Verify the serial connection {#serial}

Make sure that you can view the serial logs:

1. Open Fuchsia's serial console:

   ```posix-terminal
   fx serial
   ```

   Tip: If `fx serial` detects multiple USB devices and you don't know which one
   to use, try disconnecting the serial cable from your host, running
   `ls /dev/ttyUSB*`, then re-connecting the serial cable and running the
   command again. If you see no difference when running `ls /dev/ttyUSB*` try
   `ls /dev/tty*` or `ls /dev/*` instead.

1. Press the reset button on the VIM3. The reset button is the one with the
   **R** printed next to it on the circuit board. See [VIM3/3L Hardware] for a
   diagram. In your serial console you should see human-readable logs.

See [Troubleshooting: Bootloops] if your VIM3 seems to keep rebooting.

### Erase the eMMC {#emmc}

Before you can install Fuchsia, you need to get the VIM3 firmware and software
to a known-good state. The first step is to erase the eMMC.

1. Press the reset button on your VIM3.

1. Right after you press the reset button, start repeatedly pressing the
   <kbd>Space</kbd> key as your VIM3 boots up. Make sure that your cursor is
   focused on your serial console. The bootloader process should pause and your
   serial console should show a `kvim3#` prompt. Your serial console is now
   providing you access to the **U-Boot shell**.

1. Run the following command in the U-Boot shell:

   ```posix-terminal
   store init 3
   ```

   Your serial console logs should verify that the eMMC was correctly erased.

See [Erase eMMC] for more details.

### Update the Android image on the VIM3 {#android}

<!-- Context: https://forum.khadas.com/t/unable-to-change-bootloader-for-vim3/12708/6 -->

Now you need to get the VIM3 firmware and software to a known-good state:

1. Click the following URL to download an Android image that is known to work
   well with subsequent Fuchsia installations:
   <https://dl.khadas.com/firmware/vim3/android/VIM3_Pie_V211220.7z>

1. Extract the compressed archive file (`VIM3_Pie_V211220.7z`). After the
   extraction you should have a `VIM3_Pie_V211220` directory with an
   `update.img` file in it.

1. Follow the instructions in [Install OS into eMMC]. When running
   `aml-burn-tool` the value for the `-i` flag should be the path to your
   `update.img` file. Your command should look similar to this:

   ```posix-terminal
   aml-burn-tool -b VIM3 -i ~/Downloads/VIM3_Pie_V211220/update.img
   ```

   Tip: The `TST Mode` workflow is probably the easiest and fastest way to get
   your VIM3 into Upgrade Mode.

1. If the white and red LEDs on your VIM3 are off and the blue LED is on, it
   means that your VIM3 is in sleep mode. Try putting your VIM3 back into
   [Upgrade Mode] and then pressing the reset button again.

At this point the white LED on your VIM3 should be on and you should see logs in
your serial console after you press the reset button on your VIM3.

### Update the bootloader {#bootloader}

This section explains how to flash a prebuilt version of Fuchsia's modified
U-Boot onto the VIM3. See the following link if you would prefer to build the
modified U-Boot from source:
<https://third-party-mirror.googlesource.com/u-boot/+/refs/heads/vim3>

1. Access the U-Boot shell again by pressing the reset button and then
   repeatedly pressing the <kbd>Space</kbd> key in your serial console. When
   your serial console shows the `kvim3#` prompt, you're in the U-Boot shell.

1. In your U-Boot shell run the following command:

   ```posix-terminal
   fastboot
   ```

   You should see the following logs in your serial console:

   ```
   g_dnl_register: g_dnl_driver.name = usb_dnl_fastboot

   USB RESET
   SPEED ENUM

   USB RESET
   SPEED ENUM
   ```

   If you see the first line
   (`g_dnl_register: g_dnl_driver.name = usb_dnl_fastboot`) but not the lines
   after that, try using a different USB-C to USB-\* cable and make sure that
   it supports both data and power delivery.

1. Open a new terminal window in your host and run the following commands:

   ```posix-terminal
   cd ~/fuchsia/prebuilt/third_party/fastboot

   ./fastboot flashing unlock

   ./fastboot flashing unlock_critical

   ./fastboot flash bootloader ~/fuchsia/prebuilt/third_party/firmware/vim3/u-boot.bin.unsigned

   ./fastboot reboot bootloader
   ```

   Important: When working with Fuchsia, remember to use the
   [in-tree][glossary.in-tree] version of `fastboot` at
   `~/fuchsia/prebuilt/third_party/fastboot/fastboot`. The `fastboot` protocol
   allows arbitrary vendor protocol extensions and Fuchsia may rely on this
   functionality in the future.


### Flash Fuchsia into the eMMC (one-time only) {#fuchsia}

Use this workflow only the first time you flash Fuchsia onto the VIM3. If you've
already got Fuchsia running on the VIM3, use the [Update your Fuchsia image]
workflow because it's faster.

1. If you just ran the `./fastboot reboot bootloader` command from the last
   section then your VIM3 should already be in `fastboot` mode. You can check
   your `fx serial` logs to confirm. Otherwise press the reset button and then
   repeatedly press the <kbd>f</kbd> key in your `fx serial` console until you
   see `USB RESET` and `SPEED ENUM` again.

   Caution: You have to press the <kbd>f</kbd> key now to enter
   <code>fastboot</code> mode. Previously you pressed the <kbd>Space</kbd> key.

1. From a separate terminal on your host run the following command:

   ```posix-terminal
   fx flash
   ```

Your VIM3 is now running Fuchsia!

## Update your Fuchsia image {#update}

Complete these steps when you already have Fuchsia running on your VIM3 and want
to update the Fuchsia image.

1. Run the following command from a terminal on your host:

   ```posix-terminal
   fx serve
   ```

   Leave this command running.

1. Make some changes in your in-tree Fuchsia checkout and build the changes.

1. Open a new terminal window and perform an OTA update of the Fuchsia image on
   your VIM3:

   ```posix-terminal
   fx ota
   ```

## Reduce VIM3 noise by disabling its fan {#fan}

If the VIM3 fan's loud noise is bothering you, you can disable it with any
of the following workflows:

* Add `--args vim3_mcu_fan_default_level=0` to your `fx set` invocation.
* Add `vim3_mcu_fan_default_level=0` to `~/fuchsia/local/args.gn` if you use
  [persistent local build arguments].

Caution: Disabling the fan interferes with thermal throttling, which will impact
performance modeling and benchmarking.

Experimental: This GN flag will get disabled at some point. Your local build
will break when that happens.

## Ensure that VIM3 tryjobs run {#tryjobs}

Include the following line in your commit message to ensure that VIM3 tryjobs run:

```
Cq-Include-Trybots: luci.turquoise.global.try:bringup.vim3-debug,core.vim3-debug,core.vim3-vg-debug
```

## Troubleshooting {#troubleshooting}

This section explains workarounds for common issues.

### Troubleshooting: Bootloops {#bootloops}

Problem:

You're looking at the VIM3 serial logs. The logs suggest that the VIM3
keeps rebooting.

Root cause:

Unknown. There appears to be an underlying bug in Khada's power delivery
implementation. Fuchsia addressed the bug in [issue 122113].

Workaround 1:

Un-plug and re-plug the USB cable. Repeat 2-3 times if necessary.

Workaround 2:

Always use a USB-C to USB-A cable.

### Troubleshooting: Hardware mismatch {#mismatch}

Problem:

This error when flashing fuchsia: `Hardware mismatch! Trying to flash images
built for vim3 but have 0`

Solution:

Go back to [Update the Android image](#android) step.

## Appendix: Fix a bricked VIM3 {#brickfix}

Do these steps if you've [bricked] your VIM3 and need to "factory reset" it:

1. [Erase the eMMC](#emmc).
1. [Update the Android image](#android).
1. [Update the bootloader](#bootloader).
1. [Flash Fuchsia into the eMMC (one-time only)](#fuchsia).

## Appendix: Support {#support}

* For issues that seem related to VIM3 hardware or firmware, try the
  [VIM3 official docs](https://docs.khadas.com/linux/vim3/index.html) and
  [Khadas VIM3 official forum](https://forum.khadas.com/c/khadas-vim3/30).
* For issues that seem related to Fuchsia, try the
  [Fuchsia mailing lists and chat rooms](/docs/contribute/community/mailing-lists.md).

## Appendix: Feature support {#features}

Fuchsia currently supports these features of the VIM3:

* UART Serial Debugger
* Paving over ethernet and USB
* Storage (eMMC)
* HDMI Display and Framebuffer
* GPU (Mali) and Vulkan graphics
* Ethernet
* SDIO
* I2C
* GPIO
* Temperature Sensors and DVFS
* RTC
* Clock
* Fan
* NNA
* USB-C in peripheral mode
* USB-A
* Audio[^1]

[^1]: VIM3 does not include transducers like speakers and microphones,
    in addition to the transducers, external hardware including DACs/ADCs
    need to be added and integrated via the GPIO header to be able to
    playback and capture audio this way.

These features are under development and may not be supported:

* Video decoder
* SPI

The following features are not supported, but might be added by future
contributions:

* SPI Flash
* USB-C in host mode
* Power management and PMIC
* Wake on LAN
* UART BT

These features are not supported and are unlikely to be added:

* Video encoding (due to non-public firmware)
* Trusted Execution Environment / secure boot

## Appendix: Update the boot splash screen {#splash}

To update the boot splash screen to be the Fuchsia logo, run the following
command from a host terminal while the VIM3 is in `fastboot` mode:

```posix-terminal
~/fuchsia/prebuilt/third_party/fastboot/fastboot flash logo \
    ~/fuchsia/zircon/kernel/target/arm64/board/vim3/firmware/logo.img
```

[Khadas VIM3]: https://www.khadas.com/vim3
[install Fuchsia on a VIM3]: #install
[contributors]: /docs/contribute/community/contributor-roles.md#member
[DVFS]: https://en.wikipedia.org/wiki/Dynamic_frequency_scaling
[build Fuchsia]: #build
[USB to TTL serial cable]: https://www.adafruit.com/product/954
[heatsink]: https://www.khadas.com/product-page/new-vim-heatsink
[VIM3 collection]: https://www.khadas.com/shop?Collection=VIM3&sort=price_descending
[Download the Fuchsia source code]: /docs/get-started/get_fuchsia_source.md
[Configure and build Fuchsia]: /docs/get-started/build_fuchsia.md
[USB hub]: https://en.wikipedia.org/wiki/USB_hub
[`fastboot`]: https://en.wikipedia.org/wiki/Fastboot
[Serial Debugging Tool]: https://docs.khadas.com/products/sbc/vim3/development/setup-serial-tool
[VIM3/3L Hardware]: https://docs.khadas.com/products/sbc/vim3/hardware/start
[Erase eMMC]: https://docs.khadas.com/products/sbc/vim3/development/erase-emmc
[Install OS into eMMC]: https://docs.khadas.com/products/sbc/vim3/install-os/install-os-into-emmc-via-usb-tool#install-on-ubuntu-pc
[Upgrade Mode]: https://docs.khadas.com/products/sbc/vim3/install-os/boot-into-upgrade-mode
[bricked]: https://en.wikipedia.org/wiki/Brick_(electronics)
[Erase the eMMC]: #emmc
[Update the Android image]: #android
[Update the bootloader]: #bootloader
[Flash Fuchsia into the eMMC (one-time only)]: #fuchsia
[VIM3 official docs]: https://docs.khadas.com/linux/vim3/index.html
[Khadas VIM3 official forum]: https://forum.khadas.com/c/khadas-vim3/30
[Fuchsia mailing lists and chat rooms]: /docs/contribute/community/mailing-lists.md
[persistent local build arguments]: /docs/development/build/fx.md#defining_persistent_local_build_arguments
[Update your Fuchsia image]: #update
[Troubleshooting: Bootloops]: #bootloops
[issue 122113]: https://bugs.fuchsia.dev/p/fuchsia/issues/detail?id=122113
[bricked]: https://en.wikipedia.org/wiki/Brick_(electronics)
