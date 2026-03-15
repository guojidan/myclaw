'use client'

import { useState } from 'react'

import { approveRunPublication } from '../../../src/operator-api'

type ApprovalStatus = 'idle' | 'loading' | 'success' | 'error'

export function ApprovePublishAction({ runId }: { runId: string }) {
  const [status, setStatus] = useState<ApprovalStatus>('idle')
  const [error, setError] = useState<string | undefined>()

  const isLoading = status === 'loading'
  const isSuccess = status === 'success'

  const handleClick = async () => {
    if (isLoading || isSuccess) {
      return
    }

    setStatus('loading')
    setError(undefined)

    try {
      await approveRunPublication(runId)
      setStatus('success')
    } catch (err) {
      const message = err instanceof Error ? err.message : 'Failed to approve run publish'
      setError(message)
      setStatus('error')
    }
  }

  return (
    <div className="link-list">
      <button type="button" onClick={handleClick} disabled={isLoading || isSuccess}>
        {isLoading ? 'Approving…' : isSuccess ? 'Approved' : 'Approve publish'}
      </button>
      {error ? <p className="lede" style={{ color: '#992f1b' }}>{error}</p> : null}
      {isSuccess ? (
        <p className="lede" style={{ margin: 0 }}>
          The run was marked approved and candidate content should now be published.
        </p>
      ) : null}
    </div>
  )
}
