/*
 * Copyright (C) 2020 The Fuchsia Authors.
 * Copyright (C) 2008 The Android Open Source Project
 * All rights reserved.
 *
 * Redistribution and use in source and binary forms, with or without
 * modification, are permitted provided that the following conditions
 * are met:
 *  * Redistributions of source code must retain the above copyright
 *    notice, this list of conditions and the following disclaimer.
 *  * Redistributions in binary form must reproduce the above copyright
 *    notice, this list of conditions and the following disclaimer in
 *    the documentation and/or other materials provided with the
 *    distribution.
 *
 * THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS
 * "AS IS" AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT
 * LIMITED TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS
 * FOR A PARTICULAR PURPOSE ARE DISCLAIMED. IN NO EVENT SHALL THE
 * COPYRIGHT OWNER OR CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT,
 * INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING,
 * BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS
 * OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED
 * AND ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY,
 * OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT
 * OF THE USE OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF
 * SUCH DAMAGE.
 */

#include <ctype.h>
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/stat.h>
#include <sys/time.h>
#include <sys/types.h>
#include <unistd.h>

#include <chrono>
#include <memory>
#include <thread>

#include <linux/usb/ch9.h>
#include <linux/usbdevice_fs.h>
#include <linux/version.h>

#include "usb.h"
#if 0
#include "util.h"
#endif

double now() {
  struct timeval tv;
  gettimeofday(&tv, NULL);
  return (double)tv.tv_sec + (double)tv.tv_usec / 1000000;
}

using namespace std::chrono_literals;

#define MAX_RETRIES 2

/* Timeout in seconds for usb_wait_for_disconnect.
 * It doesn't usually take long for a device to disconnect (almost always
 * under 2 seconds) but we'll time out after 3 seconds just in case.
 */
#define WAIT_FOR_DISCONNECT_TIMEOUT 3

#ifdef TRACE_USB
#define DBG1(x...) fprintf(stderr, x)
#define DBG(x...) fprintf(stderr, x)
#else
#define DBG(x...)
#define DBG1(x...)
#endif

static void log_error(int err) {
#ifdef TRACE_USB
  char buf[256];
  const char *errstr = strerror_r(err, buf, sizeof(buf));
  DBG("%s (%d)\n", errstr, err);
#endif
}

// Kernels before 3.3 have a 16KiB transfer limit. That limit was replaced
// with a 16MiB global limit in 3.3, but each URB submitted required a
// contiguous kernel allocation, so you would get ENOMEM if you tried to
// send something larger than the biggest available contiguous kernel
// memory region. 256KiB contiguous allocations are generally not reliable
// on a device kernel that has been running for a while fragmenting its
// memory, but that shouldn't be a problem for fastboot on the host.
// In 3.6, the contiguous buffer limit was removed by allocating multiple
// 16KiB chunks and having the USB driver stitch them back together while
// transmitting using a scatter-gather list, so 256KiB bulk transfers should
// be reliable.
// 256KiB seems to work, but 1MiB bulk transfers lock up my z620 with a 3.13
// kernel.
#define MAX_USBFS_BULK_SIZE (256 * 1024)

struct usb_handle {
  char fname[64];
  int desc;
  unsigned char ep_in;
  unsigned char ep_out;
  void *callback_data;
};

class UsbInterface {
 public:
  explicit UsbInterface(std::unique_ptr<usb_handle> handle, uint32_t ms_timeout = 0)
      : handle_(std::move(handle)), ms_timeout_(ms_timeout) {}
  ~UsbInterface();

  ssize_t Read(void *data, size_t len);
  ssize_t Write(const void *data, size_t len);
  int Close();
  int Reset();
  int WaitForDisconnect();

 private:
  std::unique_ptr<usb_handle> handle_;
  const uint32_t ms_timeout_;

  // DISALLOW_COPY_AND_ASSIGN(UsbInterface);
};

class scoped_fd {
 public:
  int fd;

  scoped_fd() : fd(-EBADF) {}

  explicit scoped_fd(int fd) : fd(fd) {}

  scoped_fd(scoped_fd &&other) : fd(other.release()) {}

  ~scoped_fd() {
    if (this->fd < 0)
      return;
    ::close(this->fd);
  }

  scoped_fd &operator=(scoped_fd &&other) {
    this->close();
    this->fd = other.release();
    return *this;
  }

  int get() const { return this->fd; }

  void log_error() {
    if (this->fd >= 0)
      return;
    ::log_error(-this->fd);
  }

  int release() {
    int fd = this->fd;
    this->fd = -EBADF;
    return fd;
  }

  void close() {
    if (this->fd < 0)
      return;
    ::close(this->release());
  }

  operator bool() const { return fd >= 0; }
};

/* True if name isn't a valid name for a USB device in /sys/bus/usb/devices.
 * Device names are made up of numbers, dots, and dashes, e.g., '7-1.5'.
 * We reject interfaces (e.g., '7-1.5:1.0') and host controllers (e.g. 'usb1').
 * The name must also start with a digit, to disallow '.' and '..'
 */
static inline int badname(const char *name) {
  if (!isdigit(*name))
    return 1;
  while (*++name) {
    if (!isdigit(*name) && *name != '.' && *name != '-')
      return 1;
  }
  return 0;
}

static int check(void *_desc, int len, unsigned type, int size) {
  struct usb_descriptor_header *hdr = (struct usb_descriptor_header *)_desc;

  if (len < size)
    return -1;
  if (hdr->bLength < size)
    return -1;
  if (hdr->bLength > len)
    return -1;
  if (hdr->bDescriptorType != type)
    return -1;

  return 0;
}

static int filter_usb_device(char *sysfs_name, scoped_fd &sysfs_dir, char *ptr, int len,
                             int writable, ifc_match_func callback, void *callback_data,
                             int *ept_in_id, int *ept_out_id, int *ifc_id) {
  struct usb_device_descriptor *dev;
  struct usb_config_descriptor *cfg;
  struct usb_interface_descriptor *ifc;
  struct usb_endpoint_descriptor *ept;
  struct usb_ifc_info info;

  int in, out;
  unsigned i;
  unsigned e;

  if (check(ptr, len, USB_DT_DEVICE, USB_DT_DEVICE_SIZE))
    return -1;
  dev = (struct usb_device_descriptor *)ptr;
  len -= dev->bLength;
  ptr += dev->bLength;

  if (check(ptr, len, USB_DT_CONFIG, USB_DT_CONFIG_SIZE))
    return -1;
  cfg = (struct usb_config_descriptor *)ptr;
  len -= cfg->bLength;
  ptr += cfg->bLength;

  info.dev_vendor = dev->idVendor;
  info.dev_product = dev->idProduct;
  info.dev_class = dev->bDeviceClass;
  info.dev_subclass = dev->bDeviceSubClass;
  info.dev_protocol = dev->bDeviceProtocol;
  info.writable = writable;

  snprintf(reinterpret_cast<char *>(info.device_path), sizeof(info.device_path), "usb:%s",
           sysfs_name);

  /* Read device serial number (if there is one).
   * We read the serial number from sysfs, since it's faster and more
   * reliable than issuing a control pipe read, and also won't
   * cause problems for devices which don't like getting descriptor
   * requests while they're in the middle of flashing.
   */
  info.serial_number[0] = '\0';
  if (dev->iSerialNumber) {
    int fd = openat(sysfs_dir.get(), "serial", O_RDONLY);
    if (fd >= 0) {
      int chars_read = read(fd, info.serial_number, sizeof(info.serial_number) - 1);
      close(fd);

      if (chars_read <= 0)
        info.serial_number[0] = '\0';
      else if (info.serial_number[chars_read - 1] == '\n') {
        // strip trailing newline
        info.serial_number[chars_read - 1] = '\0';
      }
    }
  }

  for (i = 0; i < cfg->bNumInterfaces; i++) {
    while (len > 0) {
      struct usb_descriptor_header *hdr = (struct usb_descriptor_header *)ptr;
      if (check(hdr, len, USB_DT_INTERFACE, USB_DT_INTERFACE_SIZE) == 0)
        break;
      len -= hdr->bLength;
      ptr += hdr->bLength;
    }

    if (len <= 0)
      return -1;

    ifc = (struct usb_interface_descriptor *)ptr;
    len -= ifc->bLength;
    ptr += ifc->bLength;

    in = -1;
    out = -1;
    info.ifc_class = ifc->bInterfaceClass;
    info.ifc_subclass = ifc->bInterfaceSubClass;
    info.ifc_protocol = ifc->bInterfaceProtocol;

    for (e = 0; e < ifc->bNumEndpoints; e++) {
      while (len > 0) {
        struct usb_descriptor_header *hdr = (struct usb_descriptor_header *)ptr;
        if (check(hdr, len, USB_DT_ENDPOINT, USB_DT_ENDPOINT_SIZE) == 0)
          break;
        len -= hdr->bLength;
        ptr += hdr->bLength;
      }
      if (len < 0) {
        break;
      }

      ept = (struct usb_endpoint_descriptor *)ptr;
      len -= ept->bLength;
      ptr += ept->bLength;

      if ((ept->bmAttributes & USB_ENDPOINT_XFERTYPE_MASK) != USB_ENDPOINT_XFER_BULK)
        continue;

      if (ept->bEndpointAddress & USB_ENDPOINT_DIR_MASK) {
        in = ept->bEndpointAddress;
      } else {
        out = ept->bEndpointAddress;
      }

      // For USB 3.0 devices skip the SS Endpoint Companion descriptor
      if (check((struct usb_descriptor_hdr *)ptr, len, USB_DT_SS_ENDPOINT_COMP,
                USB_DT_SS_EP_COMP_SIZE) == 0) {
        len -= USB_DT_SS_EP_COMP_SIZE;
        ptr += USB_DT_SS_EP_COMP_SIZE;
      }
    }

    info.has_bulk_in = (in != -1);
    info.has_bulk_out = (out != -1);

    if (callback(&info, callback_data) == true) {
      *ept_in_id = in;
      *ept_out_id = out;
      *ifc_id = ifc->bInterfaceNumber;
      return 0;
    }
  }

  return -1;
}

static int read_sysfs_string(const char *sysfs_name, const char *sysfs_node, char *buf,
                             int bufsize) {
  char path[80];
  int fd, n;

  snprintf(path, sizeof(path), "/sys/bus/usb/devices/%s/%s", sysfs_name, sysfs_node);
  path[sizeof(path) - 1] = '\0';

  fd = open(path, O_RDONLY);
  if (fd < 0)
    return -1;

  n = read(fd, buf, bufsize - 1);
  close(fd);

  if (n < 0)
    return -1;

  buf[n] = '\0';

  return n;
}

static int read_sysfs_number(const char *sysfs_name, const char *sysfs_node) {
  char buf[16];
  int value;

  if (read_sysfs_string(sysfs_name, sysfs_node, buf, sizeof(buf)) < 0)
    return -1;

  if (sscanf(buf, "%d", &value) != 1)
    return -1;

  return value;
}

/* Given the name of a USB device in sysfs, get the name for the same
 * device in devfs. Returns 0 for success, -1 for failure.
 */
static int convert_to_devfs_name(const char *sysfs_name, char *devname, int devname_size) {
  int busnum, devnum;

  busnum = read_sysfs_number(sysfs_name, "busnum");
  if (busnum < 0)
    return -1;

  devnum = read_sysfs_number(sysfs_name, "devnum");
  if (devnum < 0)
    return -1;

  snprintf(devname, devname_size, "/dev/bus/usb/%03d/%03d", busnum, devnum);
  return 0;
}

static ssize_t read_device_descriptors(scoped_fd &sysfs_dir, void *data, size_t count) {
  scoped_fd fd(openat(sysfs_dir.get(), "descriptors", O_RDONLY));

  if (!fd)
    return fd.get();

  return read(fd.get(), data, count);
}

static std::unique_ptr<usb_handle> find_usb_device(const char *base, ifc_match_func callback,
                                                   void *callback_data) {
  std::unique_ptr<usb_handle> usb;
  char devname[64];
  char desc[1024];
  int n, in, out, ifc;

  struct dirent *de;
  int writable;

  // Explicitly give closedir's type instead of using decltype in order to avoid
  // waring about ignoring attributes (nonnull)/
  std::unique_ptr<DIR, int (*)(DIR *)> busdir(opendir(base), closedir);
  if (busdir == 0)
    return 0;

  int base_dir_fd = dirfd(busdir.get());
  if (base_dir_fd < 0) {
    DBG("Failed to get busdir as fd: ");
    log_error(-base_dir_fd);
    return 0;
  }

  while ((de = readdir(busdir.get())) && (usb == nullptr)) {
    if (badname(de->d_name))
      continue;

    scoped_fd sysfs_dir(openat(base_dir_fd, de->d_name, O_RDONLY));

    if (!sysfs_dir) {
      DBG("Failed to open device sysfs directory: ");
      sysfs_dir.log_error();
      continue;
    }

    if (!convert_to_devfs_name(de->d_name, devname, sizeof(devname))) {
      DBG("[ scanning %s ]\n", devname);
      // Check if we have read-only access, so we can give a helpful
      // diagnostic like "adb devices" does.
      if (access(devname, R_OK) != 0) {
        DBG("Cannot access %s for reading\n", devname);
        continue;
      }

      writable = access(devname, R_OK | W_OK) == 0;

      if (!writable) {
        DBG("Cannot access %s for writing\n", devname);
      }

      // Reading the cached USB descriptor is several orders of magnitude faster
      // than reading the descriptor directly from the device.
      // For example, enumerating 15 devices goes from 900ms to <1ms.
      ssize_t desc_sz = read_device_descriptors(sysfs_dir, desc, sizeof(desc));

      if (desc_sz < 0) {
        DBG("Failed to read device descriptors: ");
        log_error(static_cast<int>(-desc_sz));
        continue;
      }

      if (filter_usb_device(de->d_name, sysfs_dir, desc, desc_sz, writable, callback, callback_data,
                            &in, &out, &ifc) == 0) {
        usb.reset(new usb_handle());

        int fd = open(devname, O_RDWR);

        strcpy(usb->fname, devname);
        usb->ep_in = in;
        usb->ep_out = out;
        usb->desc = fd;

        n = ioctl(fd, USBDEVFS_CLAIMINTERFACE, &ifc);
        if (n != 0) {
          close(fd);
          usb.reset();
          continue;
        }
      }
    }
  }

  return usb;
}

UsbInterface::~UsbInterface() { Close(); }

ssize_t UsbInterface::Write(const void *_data, size_t len) {
  unsigned char *data = (unsigned char *)_data;
  unsigned count = 0;
  struct usbdevfs_bulktransfer bulk;
  int n;

  if (handle_->ep_out == 0 || handle_->desc == -1) {
    return EINVAL;
  }

  do {
    int xfer;
    xfer = (len > MAX_USBFS_BULK_SIZE) ? MAX_USBFS_BULK_SIZE : len;

    bulk.ep = handle_->ep_out;
    bulk.len = xfer;
    bulk.data = data;
    bulk.timeout = ms_timeout_;

    n = ioctl(handle_->desc, USBDEVFS_BULK, &bulk);
    if (n != xfer) {
      DBG("ERROR: n = %d, errno = %d (%s)\n", n, errno, strerror(errno));
      return -errno;
    }

    count += xfer;
    len -= xfer;
    data += xfer;
  } while (len > 0);

  return count;
}

ssize_t UsbInterface::Read(void *_data, size_t len) {
  unsigned char *data = (unsigned char *)_data;
  unsigned count = 0;
  struct usbdevfs_bulktransfer bulk;
  int n, retry;

  if (handle_->ep_in == 0 || handle_->desc == -1) {
    return -EINVAL;
  }

  while (len > 0) {
    int xfer = (len > MAX_USBFS_BULK_SIZE) ? MAX_USBFS_BULK_SIZE : len;

    bulk.ep = handle_->ep_in;
    bulk.len = xfer;
    bulk.data = data;
    bulk.timeout = ms_timeout_;
    retry = 0;

    do {
      DBG("[ usb read %d fd = %d], fname=%s\n", xfer, handle_->desc, handle_->fname);
      n = ioctl(handle_->desc, USBDEVFS_BULK, &bulk);
      DBG("[ usb read %d ] = %d, fname=%s, Retry %d \n", xfer, n, handle_->fname, retry);

      if (n < 0) {
        DBG1("ERROR: n = %d, errno = %d (%s)\n", n, errno, strerror(errno));
        if (++retry > MAX_RETRIES)
          return -errno;
        std::this_thread::sleep_for(std::chrono::milliseconds(100));
      }
    } while (n < 0);

    count += n;
    len -= n;
    data += n;

    if (n < xfer) {
      break;
    }
  }

  return count;
}

int UsbInterface::Close() {
  int fd;

  fd = handle_->desc;
  handle_->desc = -1;
  if (fd >= 0) {
    close(fd);
    DBG("[ usb closed %d ]\n", fd);
  }

  return 0;
}

int UsbInterface::Reset() {
  int ret = 0;
  // We reset the USB connection
  if ((ret = ioctl(handle_->desc, USBDEVFS_RESET, 0))) {
    return ret;
  }

  return 0;
}

UsbInterface *interface_open(ifc_match_func callback, void *callback_data, uint32_t timeout_ms) {
  std::unique_ptr<usb_handle> handle =
      find_usb_device("/sys/bus/usb/devices", callback, callback_data);
  return handle ? new UsbInterface(std::move(handle), timeout_ms) : nullptr;
}

/* Wait for the system to notice the device is gone, so that a subsequent
 * fastboot command won't try to access the device before it's rebooted.
 * Returns 0 for success, -1 for timeout.
 */
int UsbInterface::WaitForDisconnect() {
  double deadline = now() + WAIT_FOR_DISCONNECT_TIMEOUT;
  while (now() < deadline) {
    if (access(handle_->fname, F_OK))
      return 0;
    std::this_thread::sleep_for(50ms);
  }
  return -1;
}

ssize_t interface_read(UsbInterface *interface, void *data, ssize_t len) {
  return interface->Read(data, len);
}

ssize_t interface_write(UsbInterface *interface, const void *data, ssize_t len) {
  return interface->Write(data, len);
}

void interface_close(UsbInterface *interface) { delete interface; }

void interface_wait_for_disconnect(UsbInterface *interface) { interface->WaitForDisconnect(); }
