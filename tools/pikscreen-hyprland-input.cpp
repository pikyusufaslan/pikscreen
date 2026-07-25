#include <hyprland/src/plugins/PluginAPI.hpp>
#include <hyprland/src/desktop/state/LayerState.hpp>
#include <hyprland/src/helpers/time/Time.hpp>
#include <hyprland/src/managers/screenshare/ScreenshareManager.hpp>
#include <hyprland/src/pointer/PointerManager.hpp>
#include <hyprland/src/protocols/CursorShape.hpp>
#include <hyprland/src/render/OpenGL.hpp>
#include <hyprland/src/render/Renderer.hpp>
#include <hyprland/src/render/pass/Pass.hpp>
#include <hyprland/src/render/pass/SurfacePassElement.hpp>

#include <algorithm>
#include <array>
#include <cerrno>
#include <chrono>
#include <cstddef>
#include <cstdint>
#include <cstdlib>
#include <cstring>
#include <linux/input-event-codes.h>
#include <string>
#include <string_view>
#include <stdexcept>
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>

namespace {
constexpr std::array<char, 8> PACKET_MAGIC = {'Z', 'M', 'H', 'Y', 'P', 'R', '1', '\0'};
constexpr uint32_t PACKET_VERSION = 1;
constexpr uint32_t EVENT_SNAPSHOT = 0;
constexpr uint32_t EVENT_CURSOR_SHAPE = 1;
constexpr uint32_t EVENT_MOUSE_BUTTON = 2;
constexpr uint32_t EVENT_KEYBOARD_KEY = 3;
constexpr uint32_t MODIFIER_SHIFT = 1;
constexpr uint32_t MODIFIER_ALT = 2;

struct SPikScreenInputPacket {
    std::array<char, 8> magic = PACKET_MAGIC;
    uint32_t version = PACKET_VERSION;
    uint32_t size = sizeof(SPikScreenInputPacket);
    uint64_t monotonicNs = 0;
    uint32_t kind = EVENT_SNAPSHOT;
    uint32_t code = 0;
    uint32_t state = 0;
    uint32_t modifiers = 0;
    double x = 0.0;
    double y = 0.0;
    std::array<char, 64> shape = {};
};

static_assert(sizeof(SPikScreenInputPacket) == 120);

HANDLE g_handle = nullptr;
int g_socket = -1;
sockaddr_un g_target = {};
socklen_t g_targetLength = 0;
std::string g_shape = "default";
uint32_t g_modifiers = 0;
CHyprSignalListener g_tick;
CHyprSignalListener g_cursorShape;
CHyprSignalListener g_mouseMove;
CHyprSignalListener g_mouseButton;
CHyprSignalListener g_keyboardKey;
CFunctionHook* g_renderLayerHook = nullptr;
CFunctionHook* g_saveBufferForMirrorHook = nullptr;

using RenderLayerFn = void (*)(Render::IHyprRenderer*, PHLLS, PHLMONITOR,
                               const Time::steady_tp&, bool, bool);
using SaveBufferForMirrorFn = bool (*)(Render::GL::CHyprOpenGLImpl*, const CBox&);

uint64_t monotonicNs() {
    return std::chrono::duration_cast<std::chrono::nanoseconds>(
               std::chrono::steady_clock::now().time_since_epoch())
        .count();
}

void updateModifier(uint32_t keycode, bool pressed) {
    uint32_t flag = 0;
    if (keycode == KEY_LEFTSHIFT || keycode == KEY_RIGHTSHIFT)
        flag = MODIFIER_SHIFT;
    else if (keycode == KEY_LEFTALT || keycode == KEY_RIGHTALT)
        flag = MODIFIER_ALT;
    if (flag == 0)
        return;
    if (pressed)
        g_modifiers |= flag;
    else
        g_modifiers &= ~flag;
}

void sendPacket(uint32_t kind, uint32_t code = 0, uint32_t state = 0) {
    if (g_socket < 0 || !Pointer::mgr())
        return;
    const auto position = Pointer::mgr()->position();
    SPikScreenInputPacket packet;
    packet.monotonicNs = monotonicNs();
    packet.kind = kind;
    packet.code = code;
    packet.state = state;
    packet.modifiers = g_modifiers;
    packet.x = position.x;
    packet.y = position.y;
    const auto shapeLength = std::min(g_shape.size(), packet.shape.size() - 1);
    std::memcpy(packet.shape.data(), g_shape.data(), shapeLength);
    sendto(g_socket, &packet, sizeof(packet), MSG_DONTWAIT | MSG_NOSIGNAL,
           reinterpret_cast<const sockaddr*>(&g_target), g_targetLength);
}

bool prepareSocket() {
    const char* runtimeDir = std::getenv("XDG_RUNTIME_DIR");
    if (!runtimeDir)
        return false;
    const std::string path = std::string(runtimeDir) + "/pikscreen-hyprland-input-v1.sock";
    if (path.size() >= sizeof(g_target.sun_path))
        return false;
    g_socket = socket(AF_UNIX, SOCK_DGRAM | SOCK_NONBLOCK | SOCK_CLOEXEC, 0);
    if (g_socket < 0)
        return false;
    g_target.sun_family = AF_UNIX;
    std::memcpy(g_target.sun_path, path.c_str(), path.size() + 1);
    g_targetLength = static_cast<socklen_t>(offsetof(sockaddr_un, sun_path) + path.size() + 1);
    return true;
}

bool isPikScreenGuide(const PHLLS& layer) {
    return layer && layer->m_namespace == "pikscreen-guide";
}

bool monitorIsBeingCaptured(const PHLMONITOR& monitor) {
    return monitor && Screenshare::mgr()->isOutputBeingSSd(monitor);
}

void renderLayer(Render::IHyprRenderer* renderer, PHLLS layer, PHLMONITOR monitor,
                 const Time::steady_tp& time, bool popups, bool lockscreen) {
    if (monitorIsBeingCaptured(monitor) && isPikScreenGuide(layer))
        return;
    (*(RenderLayerFn)g_renderLayerHook->m_original)(renderer, layer, monitor, time,
                                                    popups, lockscreen);
}

bool ensureGuideTextureImageDescription(const SP<Render::ITexture>& texture) {
    if (!texture)
        return false;

    if (texture->m_imageDescription.expired())
        texture->m_imageDescription = NColorManagement::getDefaultImageDescription();

    return !texture->m_imageDescription.expired();
}

bool appendGuideSurfaces(Render::CRenderPass& pass, const PHLLS& layer,
                         const PHLMONITOR& monitor, const Time::steady_tp& now) {
    if (!isPikScreenGuide(layer) || layer->m_monitor.lock() != monitor || !layer->visible())
        return false;

    const auto surface = layer->wlSurface()->resource();
    if (!surface)
        return false;

    const auto position = layer->position(Desktop::View::IGeometric::GEOMETRIC_CURRENT);
    const auto size = layer->size(Desktop::View::IGeometric::GEOMETRIC_CURRENT);
    CSurfacePassElement::SRenderData data = {monitor, now, position};
    data.fadeAlpha = layer->alpha()[Desktop::View::LS_ALPHA_FADE]->value();
    data.surface = surface;
    data.w = size.x;
    data.h = size.y;
    data.pLS = layer;
    data.clipBox = CBox{0, 0, monitor->m_size.x, monitor->m_size.y}.scale(monitor->m_scale);

    bool added = false;
    surface->breadthfirst(
        [&](SP<CWLSurfaceResource> child, const Vector2D& offset, void*) {
            if (!child->m_current.texture || child->m_current.size.x < 1 ||
                child->m_current.size.y < 1)
                return;
            if (!ensureGuideTextureImageDescription(child->m_current.texture))
                return;

            data.localPos = offset;
            data.texture = child->m_current.texture;
            data.surface = child;
            data.mainSurface = child == surface;
            pass.add(makeUnique<CSurfacePassElement>(data));
            data.surfaceCounter++;
            added = true;
        },
        nullptr);
    return added;
}

bool saveBufferForMirror(Render::GL::CHyprOpenGLImpl* renderer, const CBox& box) {
    const bool saved = (*(SaveBufferForMirrorFn)g_saveBufferForMirrorHook->m_original)(
        renderer, box);
    if (!saved || !g_pHyprRenderer)
        return saved;

    const auto monitor = g_pHyprRenderer->m_renderData.pMonitor.lock();
    if (!monitorIsBeingCaptured(monitor))
        return saved;

    Render::CRenderPass physicalGuidePass;
    bool hasGuide = false;
    const auto now = Time::steadyNow();
    for (const auto& layer : Desktop::layerState()->layers()) {
        hasGuide = appendGuideSurfaces(physicalGuidePass, layer, monitor, now) || hasGuide;
    }

    if (!hasGuide)
        return saved;

    CRegion damage{0, 0, monitor->m_transformedSize.x, monitor->m_transformedSize.y};
    physicalGuidePass.render(damage);
    return saved;
}

void installGuideCaptureHooks(HANDLE handle) {
    const auto renderLayerFunctions =
        HyprlandAPI::findFunctionsByName(handle, "renderLayer");
    for (const auto& function : renderLayerFunctions) {
        if (!function.demangled.contains("IHyprRenderer::renderLayer"))
            continue;
        g_renderLayerHook = HyprlandAPI::createFunctionHook(
            handle, function.address, reinterpret_cast<void*>(renderLayer));
        break;
    }

    const auto saveMirrorFunctions =
        HyprlandAPI::findFunctionsByName(handle, "saveBufferForMirror");
    for (const auto& function : saveMirrorFunctions) {
        if (!function.demangled.contains("CHyprOpenGLImpl::saveBufferForMirror"))
            continue;
        g_saveBufferForMirrorHook = HyprlandAPI::createFunctionHook(
            handle, function.address, reinterpret_cast<void*>(saveBufferForMirror));
        break;
    }

    if (!g_renderLayerHook || !g_saveBufferForMirrorHook || !g_renderLayerHook->hook() ||
        !g_saveBufferForMirrorHook->hook())
        throw std::runtime_error("PikScreen could not install its Hyprland guide capture hooks");
}
}

APICALL EXPORT std::string PLUGIN_API_VERSION() {
    return HYPRLAND_API_VERSION;
}

APICALL EXPORT PLUGIN_DESCRIPTION_INFO PLUGIN_INIT(HANDLE handle) {
    g_handle = handle;
    if (!prepareSocket())
        throw std::runtime_error("PikScreen could not prepare its Hyprland input socket");
    installGuideCaptureHooks(handle);

    g_tick = Event::bus()->m_events.tick.listen([] { sendPacket(EVENT_SNAPSHOT); });
    g_mouseMove = Event::bus()->m_events.input.mouse.move.listen(
        [](Vector2D, Event::SCallbackInfo&) { sendPacket(EVENT_SNAPSHOT); });
    g_mouseButton = Event::bus()->m_events.input.mouse.button.listen(
        [](IPointer::SButtonEvent event, Event::SCallbackInfo&) {
            sendPacket(EVENT_MOUSE_BUTTON, event.button,
                       event.state == WL_POINTER_BUTTON_STATE_PRESSED ? 1U : 0U);
        });
    g_keyboardKey = Event::bus()->m_events.input.keyboard.key.listen(
        [](IKeyboard::SKeyEvent event, Event::SCallbackInfo&) {
            const bool pressed = event.state == WL_KEYBOARD_KEY_STATE_PRESSED;
            updateModifier(event.keycode, pressed);
            sendPacket(EVENT_KEYBOARD_KEY, event.keycode, pressed ? 1U : 0U);
        });
    if (PROTO::cursorShape) {
        g_cursorShape = PROTO::cursorShape->m_events.setShape.listen(
            [](const CCursorShapeProtocol::SSetShapeEvent& event) {
                g_shape = event.shapeName.empty() ? "default" : event.shapeName;
                sendPacket(EVENT_CURSOR_SHAPE);
            });
    }
    sendPacket(EVENT_SNAPSHOT);
    return {"pikscreen-hyprland-input",
            "Publishes input events and keeps PikScreen guides out of Hyprland captures",
            "PikScreen", "0.3.1"};
}

APICALL EXPORT void PLUGIN_EXIT() {
    g_cursorShape.reset();
    g_mouseMove.reset();
    g_mouseButton.reset();
    g_keyboardKey.reset();
    g_tick.reset();
    g_renderLayerHook = nullptr;
    g_saveBufferForMirrorHook = nullptr;
    if (g_socket >= 0)
        close(g_socket);
    g_socket = -1;
    g_handle = nullptr;
}
