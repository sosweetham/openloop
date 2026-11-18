// The NSXPC protocol between the runtime and openloopd (docs/ARCHITECTURE.md § Runtime ↔ openloopd).
// NSXPCInterface accepts only protocol metadata emitted by clang, which is why this is the tree's
// one Objective-C file (AI.md § Stack). It declares; Rust implements and calls (src/control).
// A signature change means changing its reply encoding in src/control/encoding.rs and bumping
// openloop_ipc::IPC_VERSION.

#import <Foundation/Foundation.h>
#import <Metal/Metal.h>

// Exported by openloopd. Every reply leads with a openloop_ipc::Status; unless it is Ok, the other
// reply arguments are zero or nil.
@protocol OpenLoopdService

// headset: the bytes of a openloop_ipc::HeadsetInfo, or nil while no headset is connected.
- (void)helloWithIPCVersion:(uint32_t)ipcVersion
                application:(NSString *)application
                     engine:(NSString *)engine
                 apiVersion:(uint64_t)apiVersion
                      reply:(void (^)(uint32_t status, uint32_t ipcVersion, uint64_t registryID,
                                      uint64_t bootID, NSData *headset))reply;

- (void)headsetWithReply:(void (^)(uint32_t status, NSData *headset))reply;

// region: an xpc_shmem object holding a openloop_ipc::SessionLayout. The events stay nil until
// composition moves into openloopd (R2 S3).
- (void)createSessionWithReply:(void (^)(uint32_t status, uint64_t session, xpc_object_t region,
                                         MTLSharedEventHandle *appReady,
                                         MTLSharedEventHandle *compositorDone))reply;
- (void)beginSession:(uint64_t)session reply:(void (^)(uint32_t status))reply;
- (void)endSession:(uint64_t)session reply:(void (^)(uint32_t status))reply;
- (void)destroySession:(uint64_t)session reply:(void (^)(uint32_t status))reply;

// R2 S3: openloopd allocates the images with newSharedTextureWithDescriptor:.
- (void)createSwapchainForSession:(uint64_t)session
                      pixelFormat:(uint64_t)pixelFormat
                            width:(uint32_t)width
                           height:(uint32_t)height
                      arrayLength:(uint32_t)arrayLength
                 mipmapLevelCount:(uint32_t)mipmapLevelCount
                            usage:(uint64_t)usage
                            reply:(void (^)(uint32_t status, uint64_t swapchain,
                                            NSArray<MTLSharedTextureHandle *> *images))reply;
- (void)destroySwapchain:(uint64_t)swapchain
              forSession:(uint64_t)session
                   reply:(void (^)(uint32_t status))reply;

@end

Protocol *openloop_ipc_openloopd_service(void) { return @protocol(OpenLoopdService); }
