#ifdef __aarch64__
struct stat {
  dev_t st_dev;
  ino_t st_ino;
  mode_t st_mode;
  nlink_t st_nlink;
  uid_t st_uid;
  gid_t st_gid;
  dev_t st_rdev;
  unsigned long __pad;
  off_t st_size;
  blksize_t st_blksize;
  int __pad2;
  blkcnt_t st_blocks;
  struct timespec st_atim;
  struct timespec st_mtim;
  struct timespec st_ctim;
  unsigned __unused1[2];
};
#else
struct stat {
  dev_t st_dev;
  ino_t st_ino;
  nlink_t st_nlink;

  mode_t st_mode;
  uid_t st_uid;
  gid_t st_gid;
  unsigned int __pad0;
  dev_t st_rdev;
  off_t st_size;
  blksize_t st_blksize;
  blkcnt_t st_blocks;

  struct timespec st_atim;
  struct timespec st_mtim;
  struct timespec st_ctim;
  long __unused1[3];
};
#endif
