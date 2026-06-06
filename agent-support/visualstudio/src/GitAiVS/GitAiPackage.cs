using System;
using System.ComponentModel.Composition;
using System.Diagnostics;
using System.Runtime.InteropServices;
using System.Threading;
using System.Threading.Tasks;
using GitAiVS.Listeners;
using GitAiVS.Services;
using Microsoft.VisualStudio;
using Microsoft.VisualStudio.Shell;
using Microsoft.VisualStudio.Shell.Interop;

namespace GitAiVS
{
    /// <summary>
    /// The main package entry point for the git-ai Visual Studio extension.
    /// 
    /// Responsibilities:
    ///   - Resolve the git-ai binary on startup
    ///   - Wire CheckpointService into the TextBufferListener (MEF-exported)
    ///   - Subscribe to Running Document Table events for save-based known_human checkpoints
    ///   - Show an info bar if git-ai is not installed
    /// </summary>
    [PackageRegistration(UseManagedResourcesOnly = true, AllowsBackgroundLoading = true)]
    [ProvideAutoLoad(VSConstants.UICONTEXT.SolutionExists_string, PackageAutoLoadFlags.BackgroundLoad)]
    [Guid(PackageGuidString)]
    public sealed class GitAiPackage : AsyncPackage
    {
        public const string PackageGuidString = "B2C3D4E5-F6A7-8901-BCDE-F12345678901";

        private BinaryResolver? _binaryResolver;
        private CheckpointService? _checkpointService;
        private DocumentSaveListener? _saveListener;
        private uint _rdtCookie;

        protected override async Task InitializeAsync(CancellationToken cancellationToken, IProgress<ServiceProgressData> progress)
        {
            await base.InitializeAsync(cancellationToken, progress);
            await JoinableTaskFactory.SwitchToMainThreadAsync(cancellationToken);

            Debug.WriteLine("[git-ai] GitAiPackage initializing...");

            _binaryResolver = new BinaryResolver();
            var binaryPath = _binaryResolver.Resolve();

            if (binaryPath == null)
            {
                Debug.WriteLine("[git-ai] git-ai binary not found. Extension will be inactive.");
                ShowInfoBar("git-ai is not installed. Visit https://usegitai.com to install it.");
                return;
            }

            Debug.WriteLine($"[git-ai] Found git-ai at {binaryPath} (version {_binaryResolver.ResolvedVersion})");

            _checkpointService = new CheckpointService(_binaryResolver);

            WireTextBufferListener();
            SubscribeToSaveEvents();

            Debug.WriteLine("[git-ai] GitAiPackage initialized successfully.");
        }

        /// <summary>
        /// Find the MEF-exported TextBufferListener and inject our CheckpointService.
        /// </summary>
        private void WireTextBufferListener()
        {
            ThreadHelper.ThrowIfNotOnUIThread();

            var componentModel = GetService(typeof(Microsoft.VisualStudio.ComponentModelHost.SComponentModel))
                as Microsoft.VisualStudio.ComponentModelHost.IComponentModel;

            if (componentModel == null)
            {
                Debug.WriteLine("[git-ai] Could not get component model to wire TextBufferListener");
                return;
            }

            var listener = componentModel.DefaultExportProvider
                .GetExportedValueOrDefault<TextBufferListener>();

            if (listener != null)
            {
                listener.CheckpointSvc = _checkpointService;
                Debug.WriteLine("[git-ai] TextBufferListener wired with CheckpointService");
            }
            else
            {
                Debug.WriteLine("[git-ai] TextBufferListener not found in MEF exports");
            }
        }

        /// <summary>
        /// Subscribe to the Running Document Table to get save events.
        /// </summary>
        private void SubscribeToSaveEvents()
        {
            ThreadHelper.ThrowIfNotOnUIThread();

            var rdt = GetService(typeof(SVsRunningDocumentTable)) as IVsRunningDocumentTable;
            if (rdt == null)
            {
                Debug.WriteLine("[git-ai] Could not get Running Document Table");
                return;
            }

            var vsVersion = GetVisualStudioVersion();
            const string extensionVersion = "0.1.0";

            _saveListener = new DocumentSaveListener(_checkpointService!, vsVersion, extensionVersion);

            var rdtEvents = new RdtSaveEventSink(_saveListener, rdt);
            rdt.AdviseRunningDocTableEvents(rdtEvents, out _rdtCookie);

            Debug.WriteLine("[git-ai] Subscribed to document save events");
        }

        private static string GetVisualStudioVersion()
        {
            try
            {
                ThreadHelper.ThrowIfNotOnUIThread();
                var shell = Package.GetGlobalService(typeof(SVsShell)) as IVsShell;
                if (shell != null)
                {
                    shell.GetProperty((int)__VSSPROPID5.VSSPROPID_ReleaseVersion, out var version);
                    return version?.ToString() ?? "unknown";
                }
            }
            catch
            {
                // Best-effort
            }

            return "unknown";
        }

        private void ShowInfoBar(string message)
        {
            Debug.WriteLine($"[git-ai] Info: {message}");
            // TODO: Implement VS info bar notification via IVsInfoBarUIFactory
        }

        protected override void Dispose(bool disposing)
        {
            if (disposing)
            {
                _saveListener?.Dispose();

                if (_rdtCookie != 0)
                {
                    ThreadHelper.ThrowIfNotOnUIThread();
                    var rdt = GetService(typeof(SVsRunningDocumentTable)) as IVsRunningDocumentTable;
                    rdt?.UnadviseRunningDocTableEvents(_rdtCookie);
                }
            }

            base.Dispose(disposing);
        }
    }

    /// <summary>
    /// Bridges IVsRunningDocTableEvents3 to our DocumentSaveListener.
    /// Only OnAfterSave is meaningful; all other events are no-ops.
    /// </summary>
    internal sealed class RdtSaveEventSink : IVsRunningDocTableEvents3
    {
        private readonly DocumentSaveListener _listener;
        private readonly IVsRunningDocumentTable _rdt;

        public RdtSaveEventSink(DocumentSaveListener listener, IVsRunningDocumentTable rdt)
        {
            _listener = listener;
            _rdt = rdt;
        }

        public int OnAfterSave(uint docCookie)
        {
            Microsoft.VisualStudio.Shell.ThreadHelper.ThrowIfNotOnUIThread();

            var filePath = GetDocumentPath(docCookie);
            if (filePath != null)
                _listener.OnDocumentSaved(filePath);

            return VSConstants.S_OK;
        }

        private string? GetDocumentPath(uint docCookie)
        {
            Microsoft.VisualStudio.Shell.ThreadHelper.ThrowIfNotOnUIThread();

            _rdt.GetDocumentInfo(
                docCookie,
                out _,         // pgrfRDTFlags
                out _,         // pdwReadLocks
                out _,         // pdwEditLocks
                out var path,  // pbstrMkDocument
                out _,         // ppHier
                out _,         // pitemid
                out _);        // ppunkDocData

            return path;
        }

        // Required interface members — all no-ops except OnAfterSave
        public int OnAfterFirstDocumentLock(uint docCookie, uint dwRDTLockType, uint dwReadLocksRemaining, uint dwEditLocksRemaining) => VSConstants.S_OK;
        public int OnBeforeLastDocumentUnlock(uint docCookie, uint dwRDTLockType, uint dwReadLocksRemaining, uint dwEditLocksRemaining) => VSConstants.S_OK;
        public int OnAfterAttributeChange(uint docCookie, uint grfAttribs) => VSConstants.S_OK;
        public int OnBeforeDocumentWindowShow(uint docCookie, int fFirstShow, IVsWindowFrame pFrame) => VSConstants.S_OK;
        public int OnAfterDocumentWindowHide(uint docCookie, IVsWindowFrame pFrame) => VSConstants.S_OK;
        public int OnAfterAttributeChangeEx(uint docCookie, uint grfAttribs, IVsHierarchy pHierOld, uint itemidOld, string pszMkDocumentOld, IVsHierarchy pHierNew, uint itemidNew, string pszMkDocumentNew) => VSConstants.S_OK;
        public int OnBeforeSave(uint docCookie) => VSConstants.S_OK;
    }
}
