#
# ufetch-core —— OpenWrt 包定义（只打包预编译的 ufetch）
#
# 设计取舍：
#   Rust 由 CI / 本机 cargo 交叉编译成静态 musl 二进制后放进
#   ./root/usr/bin/ufetch，OpenWrt SDK 只负责包结构与 ipk/apk 生成。
#   SDK 的 buildroot 默认没有 Rust 交叉工具链，在 Build/Compile 里跑
#   cargo 既慢又容易把 SDK 的坑带进来。
#
# 用法（在 OpenWrt SDK 根目录）：
#   cp -r <this dir> package/ufetch-core
#   make package/ufetch-core/compile V=s
#
# 打包前：
#   mkdir -p root/usr/bin
#   cp target/aarch64-unknown-linux-musl/release/ufetch root/usr/bin/
#

include $(TOPDIR)/rules.mk

PKG_NAME:=ufetch-core
PKG_VERSION:=0.1.0
PKG_RELEASE:=1

PKG_MAINTAINER:=ufetch-core contributors
PKG_LICENSE:=LGPL-3.0-or-later
PKG_LICENSE_FILES:=LICENSE LICENSE.GPL-3

include $(INCLUDE_DIR)/package.mk

define Package/ufetch-core
	SECTION:=utils
	CATEGORY:=Utilities
	TITLE:=HTTP segmented download engine for plugin self-update
	DEPENDS:=+ca-bundle
endef

define Package/ufetch-core/description
	Small Rust/Tokio HTTP download engine for plugin self-update.
	Range-based multi-connection download, adaptive ramp-up, slow-segment
	stealing, mirror fallback, resumable .parts.json sidecar, checksum
	verification and a JSON status/control interface for LuCI/rpcd.
endef

# 二进制由 cargo 交叉编译产生，这里只校验存在性。
define Build/Prepare
	@if [ ! -x ./root/usr/bin/ufetch ]; then \
		echo "ERROR: missing prebuilt ./root/usr/bin/ufetch"; \
		echo "       build it first: cargo build --release --target <triple>"; \
		exit 1; \
	fi
endef

define Build/Configure
endef

define Build/Compile
endef

define Package/ufetch-core/install
	$(INSTALL_DIR) $(1)/usr/bin
	$(INSTALL_BIN) ./root/usr/bin/ufetch $(1)/usr/bin/ufetch

	# 学习数据默认放 /tmp（重启即清）；想跨重启保留可改到 /etc/ufetch。
	$(INSTALL_DIR) $(1)/etc/ufetch
endef

$(eval $(call BuildPackage,ufetch-core))
