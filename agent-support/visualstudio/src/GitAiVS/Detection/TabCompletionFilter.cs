using System;
using Microsoft.VisualStudio;
using Microsoft.VisualStudio.OLE.Interop;
using Microsoft.VisualStudio.TextManager.Interop;

namespace GitAiVS.Detection
{
    /// <summary>
    /// Command filter that intercepts Tab key presses to detect inline completion accepts.
    /// 
    /// When the user presses Tab and an inline Copilot suggestion is visible, the next
    /// ITextBuffer.Changed event is highly likely to be an AI edit. We set a short-lived
    /// flag that TextBufferListener checks as a supplementary signal alongside stack trace analysis.
    /// </summary>
    public sealed class TabCompletionFilter : IOleCommandTarget
    {
        private IOleCommandTarget? _nextTarget;
        private DateTime _tabAcceptedAt = DateTime.MinValue;
        private static readonly TimeSpan TabWindow = TimeSpan.FromMilliseconds(200);

        /// <summary>
        /// True if a Tab key was pressed within the last 200ms.
        /// Used as a supplementary signal for AI edit detection.
        /// </summary>
        public bool WasRecentTabAccept => (DateTime.UtcNow - _tabAcceptedAt) < TabWindow;

        /// <summary>
        /// Attach this filter to a text view's command chain.
        /// </summary>
        public void AttachToView(IVsTextView textView)
        {
            textView.AddCommandFilter(this, out _nextTarget);
        }

        public int QueryStatus(ref Guid pguidCmdGroup, uint cCmds, OLECMD[] prgCmds, IntPtr pCmdText)
        {
            Microsoft.VisualStudio.Shell.ThreadHelper.ThrowIfNotOnUIThread();
            if (_nextTarget != null)
                return _nextTarget.QueryStatus(ref pguidCmdGroup, cCmds, prgCmds, pCmdText);

            return (int)Constants.OLECMDERR_E_NOTSUPPORTED;
        }

        public int Exec(ref Guid pguidCmdGroup, uint nCmdID, uint nCmdexecopt, IntPtr pvaIn, IntPtr pvaOut)
        {
            Microsoft.VisualStudio.Shell.ThreadHelper.ThrowIfNotOnUIThread();
            if (pguidCmdGroup == VSConstants.VSStd2K
                && nCmdID == (uint)VSConstants.VSStd2KCmdID.TAB)
            {
                _tabAcceptedAt = DateTime.UtcNow;
            }

            if (_nextTarget != null)
                return _nextTarget.Exec(ref pguidCmdGroup, nCmdID, nCmdexecopt, pvaIn, pvaOut);

            return VSConstants.S_OK;
        }
    }
}
